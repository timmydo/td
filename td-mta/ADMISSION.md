# Disk admission, bounded work and request retention

PostgreSQL owns durable mail quotas, transaction serialization, snapshot
lifetimes and commit outcomes as specified in [POSTGRES.md](POSTGRES.md).
The removed embedded-store coordinator and its physical page/WAL quotas are
not service alternatives. [STORAGE.md](STORAGE.md) owns local runtime and
request-file custody.

`admission.rs` validates an immutable plan without inspecting free space.
Generic fixed logical leases, effect tickets and work meters remain shared
primitives. The active `ResponseCoordinator` uses them for process-local
response retention only; they are not a second database quota ledger.
PostgreSQL server disk and memory limits require server administration.

## 1. Disk accounting

Lengths, offsets, quotas and arithmetic use checked u64. Process response
files reserve their entire admitted allowance before creation. Occupied,
failed-cleanup and retired slots remain charged; a failed cleanup cannot
claim reusable capacity. These logical reservations do not reserve physical
filesystem space or guarantee successful writes.

The configuration retains body, blob, queue, response, cache and work settings
shared by request planning. PostgreSQL adapters enforce their own transaction
and account limits under the original operation deadline. See POSTGRES.md
for exact enforcement and uncertainty boundaries.

## 2. Mutation publication

Durable body bytes and metadata commit together in PostgreSQL. Authentication,
configuration selection and publication checks remain transaction-bound.
A confirmed response requires a known durable result. Unknown commit outcomes
remain uncertain and must not be blindly repeated. Local response retention
reserves complete output before mutation and preserves that distinction.

## 3. Work budgets and deadlines

The shared WorkLimits plan supplies bounded foreground, change, request,
commit, maintenance and admission intervals. Its conservative final-commit
timer includes the retained checkpoint scheduling allowance; this is a
process timeout budget, not a PostgreSQL checkpoint setting. Database work
limits and cancellation are defined in POSTGRES.md. Neither retry nor chunk
boundaries can renew an operation's original deadline. MIME and response
meters separately charge bytes, records, scheduling turns and output.

### Network timers

Both idle timers and absolute operation deadlines are enforced. Progress resets
only the applicable idle timer. Main's scheduling wait remains at most five
milliseconds, not a five-millisecond network timeout.

| Operation | Default timing policy |
| --- | --- |
| TLS handshake | 10 seconds total |
| Mail-routing configured-name DNS lookup | 5 seconds total across UDP/TCP/CNAME work |
| PostgreSQL writer-name lookup | 5 seconds total in the disposable helper; POSTGRES.md section 10 |
| TCP dial | 10 seconds total across returned addresses |
| HTTP headers | 15 seconds idle, 30 seconds total |
| HTTP request/upload body | 15 seconds idle; total derived from admitted bytes below |
| HTTP response/download | 15 seconds idle; total derived from retained/download bytes below |
| HTTP keepalive | 15 seconds idle, at most 100 requests per connection |
| Event stream | One hour maximum; no input-idle timer while subscribed; 15-second output-stall timer only while an event/ping is pending |
| Inbound SMTP | 300 seconds command/DATA idle; DATA 1800 seconds total; at most 100 transactions per connection |
| Smart-host SMTP | 300 seconds greeting/ordinary-command reply, 120 seconds DATA initiation, 180 seconds per 16 KiB DATA write block, 600 seconds final-DATA reply |
| Outbound attempt | Checked sum of bounded command/block budgets, defined below |
| Online ACME transport lease | 60 seconds absolute; release between polls/requests as RESOURCES.md specifies |
| Offline migration page/blob transfer | 15 seconds idle; total max(1800 seconds, derived HTTP transfer budget) |
| HTTP-01 | One request, five seconds absolute, one slot per peer, as RESOURCES.md specifies |

Timers may be raised to at most 16 times their default; lowering them is not a
v1 option. The 60-second online control lease, five-second HTTP-01 lifetime,
one-hour event lifetime and connection request/transaction counts are fixed.
The timer plan stores each configurable phase separately. The 120-second
HTTP transfer minimum and 30-second base term can each be raised up to 16
times their defaults; the configured minimum rate is 4096..65536 bytes/second.
The offline 1800-second transfer minimum is separately configurable in its
16-times range. A checked helper converts seconds to absolute Tick deadlines;
the caller applies every enclosing lease and does not renew absolute timers.
These helpers calculate budgets only; protocol state machines own phase
transitions, progress/idle handling, fixed lifetimes and operation counts.

Event closeafter/ping follow RFC 8620 within that lifetime; quiet subscriptions
are not timed out as idle HTTP bodies. Inbound SMTP session lifetime is also
one hour; prefer closing at a transaction boundary and never acknowledge
incomplete DATA on shutdown/resource expiry.

The receiving `smtp_network` driver applies SMTP idle and DATA total
budgets and the fixed connection lifetime. Dispatched worker time does
not count as client idleness; completion restarts the idle timer.
DATA/session deadlines never renew, and every worker additionally
applies its storage/handshake execution budget. Accepted MAIL commands
count toward the transaction ceiling across protocol resets.

A known final DATA result receives up to five seconds to finish, even if
the old DATA or session work lease expired while awaiting completion.
Every finishing allowance is capped at five seconds after the original
one-hour session deadline. Plaintext and TLS use that fixed transport
cap, established at connection creation; late completion cannot renew
it. A result arriving at or after that cap cannot be acknowledged, but
stays committed. This allowance never authorizes storage work. After
session expiry, the final result and following 421 share that allowance
with shutdown/drain. A worker refusal closing the connection, or expiry
at a legal command boundary, likewise shares five seconds for its reply
and closure. Partial commands or incomplete DATA abort on expiry. Clock
or transport failure can still prevent an acknowledgement; the stored
outcome remains unchanged.

Other close replies flush under the ordinary reply deadline, then write
shutdown and input discard share up to five seconds within the fixed
transport cap. Additional reads stop after 64 KiB or EOF; buffered tails
are discarded immediately. This bounds shutdown work and is best effort,
not a guarantee that the peer receives a refusal: unread input can still
cause a reset. No discarded bytes return to parsing. The caller disposes
uncommitted jobs on workers. The direct receiving runtime enforces bounded
connection/work slots and global, listener and per-peer admission; its
retained slot reserves completion capacity before dispatch. Finalization
captures the configured commit deadline once, before queueing, and retries
keep it. Begin-DATA ticket contention may retry only before admission,
under the original queued admission cap. The PostgreSQL adapter admits four
resident DATA jobs across all listeners and accounts; the fixed storage worker
continues to serialize publication. A paused DATA sender consumes one permit,
and exhaustion waits before 354 without allocating another incoming body.

For an HTTP transfer of admitted maximum B bytes, use
`max(120 seconds, 30 seconds + ceil(B / minimum_rate))`. The default minimum
rate is 65536 bytes/second, configurable down to 4096, never zero. Use the
validated Content-Length when known; otherwise use the relevant upload/request
cap. Downloads use known blob length; retained JMAP responses use their exact
length. The absolute deadline never moves as bytes arrive. This permits a
32 MiB transfer at 64 KiB/second while still bounding slow trickles. Idle timers
remain separate. Online ACME's short control lease remains its tighter cap.

HTTP timers apply to distinct states: header receipt, body receipt, server
execution, and response transmission. After receiving a complete JMAP body,
suspend socket-idle timers while its execution deadline governs the connection.
Start transmit timers only when retained output is ready. At request admission,
set the outer exchange deadline to the checked sum of header, maximum body,
execution and maximum response-transfer budgets; never reset it at a transition.
The response maximum uses the admitted retention cap until its actual size is
known; the actual phase timer may be shorter. Socket closure is still observed
during execution; cancellation preserves committed effects.

For smart-host SMTP let N be the selected recipient count and W the exact
number of DATA wire bytes, including dot stuffing and the terminator, counted
by a bounded preflight pass over the immutable transmission file. Let K be
ceil(W / 16384), at least one. The absolute attempt lease is:

```text
dns_total + dial_total + handshake_total
+ (16 + N) * ordinary_command_timeout + data_init_timeout
+ K * data_block_timeout + final_reply_timeout
+ 4 * final_commit_allowance
```

The fixed 16 covers greeting, EHLO/STARTTLS, bounded AUTH exchanges and QUIT;
never execute unbounded extra challenges/commands under it. Four commit
allowances cover Prepared, Body, AcceptancePossible and outcome phases. This
lease derives from per-command/per-block timers as RFC 5321 requires; there is
no independent 1800-second transaction cutoff. A DATA block has an absolute
write/flush deadline, not an idle timer renewed by one-byte progress. Partial
or malformed replies cannot reset a command's deadline. Queue expiry is checked
before dispatch; already admitted attempts finish or fail under their lease.
The bound can be long on a very slow peer; one shared background-view permit
and fixed transport slot bound resource use, and administration can abort an
attempt subject to QUEUE.md's uncertainty rules.

Define `final_commit_allowance = checkpoint_total + 2 * commit_total +
admission_wait`, initially 121 seconds. Once an AcceptancePossible fence is
requested, its phase/outcome work has priority over new ordinary commits and
maintenance; at most the already running bounded writer operation may precede
it. Before the fence, hold final-result capacity and ensure remaining lease
covers the fence allowance, one terminator-block timeout, the full final-reply
timeout and the outcome allowance. Otherwise abort before the terminator.
The final reservation's deadline covers that entire remaining attempt lease;
it is not a fresh 30-second object timeout. After a final reply, process the
priority durable outcome before any new background writer work. Cooperative
clock/budget expiry never labels a known committed outcome failed; a kernel
I/O hang/storage fault can still force the defined indeterminate recovery path.

## 4. Exact JMAP result retention

One request executes methods in order. A later call may reference an earlier
result, including a query whose answer would change after an intervening Set.
Retain the actual response; never resolve a reference by rerunning the method.
This is required even when the client does not ask to return createdIds.

Use an exclusively created mode-0600 file in `tmp/requests/REQUEST/` for response
JSON, response offsets and the creation-ID map. Names are server-owned and
validated; no client string becomes a path. These are disposable private files,
not metadata records or backups, and do not need fsync. The bounded td-mail
creation overlay below is the explicit active-profile exception for creation
map storage. Charge every file byte before writing. Cleanup after completion/disconnect; abandoned files are discarded
under LOCK at startup. No response-sized heap tree or mailbox-sized map exists.

Each HTTPS slot's existing 96 KiB scratch is partitioned into 16 KiB HTTP state,
16 KiB JSON output, 16 KiB spool-walker/escaping state, 8 KiB response-index
storage, and 40 KiB argument/scheduler/framing state. At most twice json_methods
response entries exist, including implicit Email/set responses. Store name and
call-ID byte spans plus checked file offsets/lengths; strings need no duplicate
heap allocations. The walker has 160 fixed nesting frames, separately from the
input JSON-depth limit: MIME bodyStructure may add an array and object per
compiled MIME level. Reject an internal output exceeding this bound before
method effects; do not recurse on an unbounded worker stack.

Before processing any method reserve the request's mandatory framing, the
exact escaped call IDs, one bounded method-error result for every admitted call,
and the initial createdIds map and its possible response copy. Count emitted
bytes with the same checked encoder used for output. This reserve lies inside
the per-request/aggregate retention quota and is unavailable to optional body
values or search output. Startup's per-request minimum guarantees this framing
can fit; current aggregate/filesystem pressure returns HTTP 503 before effects.
The request's maxSizeRequest/maxCallsInRequest errors remain the distinct RFC
request-limit errors; spool capacity is not an invented advertised Core limit.
The fixed reservation
for server diagnostics is 512 UTF-8 bytes per diagnostic, with redacted stable
codes; response/property IDs and paths are counted separately at their full
escaped lengths. Bound the number of generated properties by the method
schema/input tokens, never an unbounded diagnostic list.

The inert `jmap_request::response::Preflight` counts mandatory wire framing
through the same bounded writer used by its all-call fallback. It owns the
admitted request and borrows the captured Session state; exact escaped call
IDs, state and the initial creation map are counted without copying them.
Its passive Plan additionally covers a second serialized seed-map copy and
one method error per actual call with a type of at most 64 ASCII letters or
digits and a diagnostic of at most 512 UTF-8 bytes. Other error properties,
index structures, created-map overlays, successes and implicit responses
need their separate capacity plans. The seed map is counted, not written
or indexed. This boundary acquires no disk reservation and performs no method
effects. Runtime must bind its cap to the actual reservation and preserve
original work/deadline admission while counting, executing and retaining.
The fallback emits ordered serverFail responses with the stable redacted
description responseCapacity; it is usable only before any method effects
for a whole-request deterministic capacity failure. One method's capacity
failure must instead produce that call's error and continue later calls.
The fallback is not a transient-pressure mapping or a way to replace earlier
committed results. It uses the captured pre-dispatch Session state; a normal
dispatcher must replan if it needs to emit a different state. The writer does
not publish HTTP headers; all fragments remain provisional until retention
completes. Cancellation and step refusals allow recovering the same arenas
with into_request. Constructor and consuming finish refusals drop them;
this new owner promises no recovery through those consuming error returns.

Before the first mutation of each method, additionally reserve its complete
possible normal per-object success/error maps, new creation-ID records/final
map output, and any implicit response. Compute the bound from actual escaped
creation/object IDs and property paths plus fixed bounded server fields. A
large client property name cannot be truncated into a misleading diagnostic
or charged as a small constant. If a bound cannot fit, fail before this method's
effects, keeping earlier responses: serverFail for a deterministic per-request
capacity/work ceiling, serverUnavailable for transient shared-pool/disk pressure
or elapsed-time exhaustion. Describe the exhausted limit in bounded redacted
text; repeated identical requests must not be told that a fixed cap may clear
merely by waiting. Do not infer determinism from a transient I/O failure.
Per-object plan/result slots are reused, but their retained bytes remain charged
until the entire request finishes.

Read-only methods write an unpublished tail. Publish a response index entry
only after the method's complete valid JSON is present. Capacity/work expiry
truncates that tail and writes the already reserved method error. An exact
total, body value, query page or /get list is never replaced by an incomplete
success. In particular maxBodyValueBytes=0 means no body-value truncation;
insufficient output capacity is an explicit method failure, not a smaller body
with a fabricated completeness flag. requestTooLarge retains its method's
specified meaning; it is not a catch-all response-size error.

If some objects of a Set have committed, finish its normal per-object result
using the reserved space where the remaining refusals have defined SetError
semantics (for example overQuota or forbidden). Do not invent a standard
per-object resource error: RFC 8620 section 5.3 does not list serverUnavailable
as a SetError. Preflight avoids predictable exhaustion before mutation. If an
unavoidable later work/deadline/resource failure prevents completion and no
truthful standard SetError applies, use method-level serverPartialFail,
requiring resynchronization of that method's affected data; preserve all earlier
method responses. Never use serverUnavailable/serverFail at method level after
its known effects. Physical spool I/O failure after effects can still prevent
any response: close the incomplete
HTTP exchange and preserve committed effects for client reconciliation. Do not
undo metadata state or fabricate failure of an already committed object. A
spool fault alone is not database corruption; report the actual failure domain.
Headers for the final JSON response are emitted only after complete request
retention succeeds, then stream its exact results and release all files/pins.

The full creation profile requires known committed creations to enter
createdIds even if their enclosing method
later returns serverPartialFail. Keep the successful submission-ID subset in
its reserved private outcome records: EmailSubmission/set still dispatches the
mandatory implicit Email/set for those successes, under the same call ID, even
after a partial primary result. Reserve both responses and hook planning before
submission effects. If time/resource exhaustion prevents the hook, its own
response reports the appropriate no-effects error or serverPartialFail according
to its actual effects; it is never silently omitted. A hook failure cannot
roll back a durable submission. The same rule applies to Email/copy's implicit
destroy-original method. Physical response-file loss still uses the connection
failure path, not fabricated results.

### Private response-file custody

RequestSpool and ResponseCoordinator::retain_response implement file custody
and quota reservation for the authenticated Core API worker. Mailbox-access
startup opens the separately provisioned private data/tmp/requests root and
holds its LOCK. Cold opening validates the complete closed namespace
before deleting abandoned responses: at most 32 server-named request
subdirectories, each containing only its optional response file. Directory
owner read/search permissions filtered by umask may be repaired to permit
child inspection after validating its type, owner and absence of group,
other or special permission bits. Remaining owner bits are restored only
after namespace validation. Unknown entries, links, wrong owners, unsafe
permissions and files above 1 GiB
refuse startup. Cleanup and initialization share the original deadline.

Before file creation, the coordinator reserves the full configured
per-request response cap in its existing ResponseBytes ledger bucket.
There is no second byte counter. A busy effect ticket owns this entire
reservation throughout file creation, execution, transmission and cleanup;
expiry cannot cancel it. Each admitted response leaves a ledger cell for
durable publication or maintenance. Default aggregate quota admits two
full response reservations; future HTTP admission must reflect this limit
and return transient pressure before method effects. Logical reservation
still does not reserve filesystem blocks or guarantee I/O.

The owner exposes bounded append, immutable-prefix publication, exact reads
of that prefix and rollback of only its unpublished tail. Each operation
is at most 64 KiB. Its original clock/deadline and the configured foreground
I/O/record work ceilings bound file work, including failed attempts and
replay. Capacity rejection before writing permits tail rollback; I/O,
clock and work failure are sticky. Dispatcher method budgets must leave
room within these enclosing limits for already reserved error output.
This raw boundary does not validate JSON, reserve method error framing,
index responses, resolve references or publish HTTP headers. Its deadline is
the original outer exchange deadline, not the shorter execution deadline;
the dispatcher must additionally enforce execution and transmission phase
budgets. The failure and work_failure accessors distinguish terminal work
exhaustion from a recoverable prewrite cap refusal. Output bytes also count
as I/O; the shared meter's output ceiling adds no independent tighter bound.

Discard/drop keeps the sole owned descriptor open through inode comparison
and unlink, then closes it before releasing quota or the slot. This prevents
inode reuse after premature close; it does not remove the stable, trusted
namespace assumption in STORAGE.md. Proven cleanup settles zero remaining
physical
response bytes, then cancels the unused logical reservation. Uncertain
cleanup settles the entire cap as used and retires the filesystem slot
until cold recovery; it cannot unlink a replacement inode or refund its
charge. Ledger settlement failure stops admission and requires recovery.
Cleanup may outlive the request deadline and waits for coordinator state
only on a worker. Forgotten owners keep their reservation and descriptor;
recovery requires process quiescence, not merely opening another root.
No fsync or durable mail authority is introduced.

### References and creation IDs

Implement RFC 8620 section 3.7 against immutable retained response bytes:

1. Find the first earlier response with the exact resultOf call ID.
2. Check that response's name against name; do not search later same-ID replies
   for a more convenient match (implicit responses share a call ID).
3. Apply JSON Pointer to its arguments, including ~0/~1 escaping, array-index
   rules and the RFC's array-only wildcard mapping/order/flattening behavior.

The inert `jmap_request::result_index::Responses` implements the first two
steps. It owns the immutable admitted request and holds at most 128 entries,
two per admitted call, in wire order without skipped calls. The selected
Core/Mail/Submission methods each require one reply plus at most one implicit
reply; an extension with another response cardinality needs new planning before
it can be advertised. This is not a universal extension-method constraint.
Each entry stores
a server-literal name of at most 64 ASCII letters/digits, slash or underscore,
a request-bound call-ID token and checked nonoverlapping arguments offsets.
Append checks each complete-object span against the supplied published prefix;
this is passive metadata, not proof of file identity or valid retained JSON.
The dispatcher must publish through the matching ResponseRetention first and
preflight index capacity before effects. No disk read is authorized by a Span.
Lookup requires every earlier call indexed and none from the current or later
calls; a behind/ahead index is State, not invalidResultReference. It borrows
and freezes the index and reference strings, compares at most one primary ID
and 256 ID bytes per step, then compares the bounded name. A mismatching
primary skips its implicit reply by token identity without rescanning bytes.
The first earlier ID match is final, even if its name differs. Empty and
repeated opaque IDs are preserved. Cancellation through into_request returns
the original request arenas; dropping the index releases them. Debug never
includes IDs or reference strings.
Runtime still charges original work/time between steps. This does not parse
reference objects, apply JSON Pointer, resolve creation IDs, bind the actual
retention owner or activate dispatch. Internal index-state errors remain
separate from invalidResultReference.

The shared [Scanner](../td-json/src/scan.rs) is the lexical/structural reader
for the retained-response walker; its API owns the fragmentation, event,
completion and error contract. Retained producers must prove unique object
keys and I-JSON scalar policy before publication. The enclosing walker binds
the source, checks span arithmetic, charges reads/work/deadlines, discards
provisional selections on error and applies the reference's Pointer rules.
The scanner alone does not read a file, select a value or activate dispatch.

The inert `jmap_request::pointer::Walker` now applies JSON Pointer plus the
RFC 8620 array wildcard to that stream. It borrows the decoded path, validates
its escapes in bounded turns, matches decoded object keys lazily without
retaining or prescanning whole segments, and accepts only canonical
nonnegative array indexes after at most 21 token bytes. A star is an
ordinary object key; only an array interprets it as a wildcard. Wildcards map
the remaining tokens over each element in order and flatten each resulting
array one level. Missing branches invalidate the entire provisional selection;
an empty wildcard returns an empty array without evaluating its remaining
tokens (their syntax is still validated). Path depth is not capped by response depth for that reason.
The walker emits passive relative byte ranges and generated array delimiters,
not copied value trees. Its complete success is required before using the
resolution; a later failure discards earlier pieces. Scanner/source-limit
errors that are detected remain distinct from invalidResultReference. The
first error wins: bad references may stop before a corrupt tail, so retained
producer integrity remains a prerequisite. Actual file identity,
checked file-base arithmetic, reads, retention of expanded results and shared
work/deadline admission remain with the enclosing dispatcher. No endpoint
uses this walker yet and it grants no file-read or publication authority.

The inert `jmap_request::resolution::Resolution` now binds the walker to
one exclusively borrowed `ResponseRetention`. Its caller supplies a source
range from the earlier-response index; construction checks that the range
lies wholly in the immutable published prefix and that no unpublished tail
exists. The caller supplies a per-expansion output ceiling that excludes its
mandatory response reserve; all delimiters and copied bytes count against it. Source reads, bounded CPU turns, copying, and final publication use
that owner's original work meter and deadline. The dispatcher must still
bind the index to this file and enforce its execution-phase deadline.

Expansion appends a separate JSON value to the same private file, publishing
only in freshly admitted finish, after complete selection succeeds. A bad
later branch or copy failure
rolls back only the unpublished tail, leaving earlier published values intact.
Drop cancels incomplete output; explicit cancellation reports rollback faults,
and any such fault remains sticky in the retention owner. Terminal work or
deadline failure can prevent truncation; the tail stays unpublished until
whole-file cleanup. Successful finish
freshly checks admission before returning the passive range. HTTP encoding
must frame indexed values, never transmit this mixed-content file wholesale.
No method dispatcher or endpoint uses this adapter yet.

The inert `jmap_request::dispatch::run` now binds a fresh retention file to
one response index while executing an admitted/preflighted batch. It resolves
top-level referenced arguments before method selection, emits Core/echo with
the resulting values, and retains ordered method errors without skipping later
calls. Missing Core in `using` makes Core/echo unknownMethod. Ordinary and
referenced forms of the same argument are invalidArguments. Error responses
are themselves eligible earlier responses; opaque duplicate call IDs retain
the first-match rule. Creation-looking strings in echo values remain ordinary
strings; the input createdIds map remains in the owned request for framing.

Optional expansions and successful arguments cannot consume the preflighted
mandatory reserve. Each expansion receives the remaining optional ceiling;
normal arguments use the shared bounded arena emitter. A recoverable optional
capacity refusal rolls back the current tail and retains a serverFail result.
Successful expansions from a subsequently failed method remain private and
charged until whole-file cleanup. Terminal original work, I/O or outer-time
failure aborts this read-only batch; it cannot be represented as a healthy
per-method capacity refusal. The shorter execution deadline uses the same
clock and meter and must precede the outer deadline; expiry retains
serverUnavailable with executionDeadline while outer time/capacity remain.
Collision validation precedes all reference resolution. A refused run drops
its request arenas and cleans up its owned retention file; recycling is not
part of this interface.

Completed now owns the response index and exact retention file, and borrows
the captured Session state. Its encoder borrows all three together, preventing file/index
substitution. It first counts the final Response envelope, then freshly admits
output, returns the Content-Length and emits exactly that many bytes. Counting shares framing and
arena emission with output but adds published argument-span lengths without
reading the file. Output reads those spans in bounded chunks and preserves
response order, escaped call IDs, Session state and initial createdIds
presence (including an empty map). Count scratch remains private.

Counting, output and final completion all use the original retention budget
and deadline. Only successful output polls expose eligible body fragments;
an output-time failure aborts the exchange rather than replacing its body.
After output starts, Completed refuses any new encoder, even after a failed
or completed stream. Before output, dropping an encoder leaves the same owned
file and budget. Dropping Completed cleans up that file. Into-request recovery releases retention and returns the
original arenas. Only side-effect-free handlers currently run: any future
creating method must first supply the final creation-ID overlay to framing.
HTTP worker handoff and authenticated discovery remain unimplemented.
No Core capability or API endpoint is advertised by this adapter.

The worker also implements the empty-registration PushSubscription policy.
Both methods require Core in using and run after result-reference resolution.
Get accepts nullable ids/properties, defaults to the nonsensitive properties,
returns no accountId/state, deduplicates requested absent generic IDs, and
refuses explicit url/keys with forbidden. Set accepts only create/update/
destroy, has no accountId/ifInState or response state fields, rejects every
creation with forbidden, and reports absent updates/destroys as notFound.
No URL is contacted and no credential registration or creation-ID entry is
created. Generic IDs exclude unresolved creation-reference spellings.
Multiple simultaneous invalid arguments have no promised error precedence.
Invalid argument shapes and unknown properties remain method errors.
The objects-per-method limit travels in the validated request's envelope:
get counts all requested IDs and set counts the sum of create/update/destroy.

Push arguments are normalized into a published, private JSON value after
reference resolution, then inspected with the shared structural scanner.
Only direct-child descriptors and bounded decoded names are retained;
nested create/update values are scanned without materializing their trees.
A resolved argument need not fit the original request arena. Normalized
arguments, reference expansions and final results all consume the same
optional retained-byte ceiling. A late validation failure leaves previously
published private arguments charged and rolls back only provisional output.

A missing target, wrong response name or failed pointer traversal is
invalidResultReference. A successfully resolved value of the wrong argument
type is invalidArguments when the method validates it. Simultaneous normal and
referenced forms of one argument are invalidArguments. Failure to read the
spool or exhaustion of work is a resource/I/O error, not a claim that the
client's reference is invalid. Resolved arguments may stream from bounded
spool views; do not require every expansion to fit the original request arena.
Apply method count/type limits after resolution, before mutation.

The creation-ID map includes the request's initial mappings and every successful
creation in order, with normal RFC collision/lookup rules. Maintain bounded
disk lookup/sort structures and charge their I/O; no full map is held in RAM.
The active td-mail creation-overlay exception below replaces the map storage
rule until creation-reference dispatch is implemented. Use indexed lookup and
a bounded append overlay, not a full linear map scan per reference; prove the largest admitted creation chain fits its work budget.
Return createdIds only when it was present in the request, including its input
mappings and new mappings. A failure/cancellation does not manufacture an ID
for an uncommitted object. Creation references and result references are
separate mechanisms and both need exact retention.

## 5. Qualification

Real PostgreSQL fixtures cover transaction publication, uncertainty, snapshots,
body integrity and client/service flows. They do not establish maximum server
resource usage or complete filesystem/power-loss qualification. Pure Rust/MIME
allocation evidence does not measure database-server memory. CONFORMANCE.md
and POSTGRES.md identify the scope of each retained fixture.

### Prepared Core Session metadata

`jmap_session::Session::core` prepares one immutable Session document for the
configured account and HTTPS origin. Its request/upload byte, method and
object limits come from the validated ResourcePlan. The caller supplies the
actual upload/API concurrency admission counts as named Concurrency fields,
retained for runtime inspection, each nonzero and no greater
than the HTTPS pool. Construction does not prove those admissions exist and
does not authenticate or activate discovery: the runtime must enforce the
same counts and finish the advertised Core contract before serving it.

Only Core is included, both server-wide and for the personal writable
account; primaryAccounts and collationAlgorithms are empty. Mail/Submission
capabilities require their own complete implementation. URL templates use
canonical configured DNS origin/port and fixed paths, never request Host.
The opaque state is SHA-256 over a version domain and every serialized field
before state, so metadata changes invalidate API sessionState. Canonically
equivalent origins produce identical documents. This state is a change token,
not authentication, account data state or a MAC.

Construction is bounded by the existing 254-byte username, 4096-byte account
name and DNS-origin limits, with shared JSON string escaping and I-JSON
noncharacter refusal. A fallibly reserved 32 KiB buffer holds even maximally
escaped fields. The complete document and its state are exposed only on
success. HTTP authentication, cache-control, worker handoff and Core endpoint
activation remain separate; discovery must not expose this preparation early.

Session concurrency limits are per endpoint, not additive reservations of
connection slots. The runtime also applies its shared HTTPS pool and event
stream ceiling; simultaneous traffic can exhaust that shared capacity.
The prepared document retains its bounded 32 KiB allocation and should be
shared for the selected configuration, rather than rebuilt per connection.
A multi-account profile retains one document per account (at most sixteen).
Authenticated account selection chooses exactly one document; the documents
share the same global request/upload admission. RESOURCES.md accounts for their
aggregate startup allocation.

### Prepared EventSource frames

`jmap_events` parses the three configured EventSource URI-template query
parameters with one percent-decoding pass. It bounds the query to 4096 bytes,
refuses missing/duplicate/unknown parameters and malformed numeric/escape
values, filters the fixed account-type roster, and ignores unknown type
names. `types=*` selects the roster; closeafter is state or no. Zero disables
pings; positive intervals clamp to 1..300 seconds, within RFC 8620's permitted
interoperability range. Plus is a literal URI-template character, not form
encoding.

A passive Snapshot contains at most four typed DataState values and one
IdentityState for one account and epoch; duplicate types or cross-account/
epoch mixtures refuse. The caller supplies only types visible under current
authorization. A stream retains passive snapshots and one 2048-byte event
frame, never a storage view. Only selected changed types enter a StateChange
map. Its opaque event id hashes the type selection and complete visible
snapshot; a reconnect
with the current id suppresses redundant initial state, while a missing or
stale id sends current selected states. This is coalesced synchronization,
not a replay of every historical transition or an authorization token. A
changed type selection cannot reuse an acknowledgement. Changes outside the
selection may cause a redundant selected-state event on reconnect, preserving
the RFC recommendation to encode all visible state.

A pending frame stays immutable until the runtime reports complete transport
flush through acknowledge. State changes arriving while it is pending are
coalesced from the caller's latest snapshot after that acknowledgement. Ping
intervals run from the last acknowledged event and ping events carry no id.
Close-after-state closes only after the state frame is acknowledged, never
on a ping. Clock regressions and account substitution refuse. Authentication,
revocation, connection limits, snapshot acquisition, socket scheduling and
stall/total deadlines remain runtime responsibilities. These helpers activate
no HTTP endpoint and do not advertise Core early.

## Active td-mail creation overlay

This profile explicitly permits a resident overlay in place of the full
creation-map disk contract above. It holds at most 64 distinct prepared keys
across Email and Mailbox creation in one request, including refused keys.
Each entry holds two fixed bounded ID buffers; a separate fixed 64-slot index
keeps sorted lookup while prepared slot numbers remain stable. This is an
additional bounded request heap allowance, separate from the fixed HTTPS
scratch partition. Growth may temporarily hold the old and new entry arrays,
so its payload peak is at most twice the 64-entry ceiling plus the index.
It cannot grow with mailbox size or the initial creation map. Reservations
charge each entry's resident metadata and worst-case output growth against
the remaining response allowance before any mutation. Resource exhaustion
returns responseCapacity before the adapter is invoked; a request may reach
this resource ceiling before its maxObjectsInSet count. Earlier successes
remain valid. Reusing a slot consumes no further map allocation or allowance.

The initial map stays in the already admitted immutable request arena; no
second resident copy or quadratic seeding pass exists. The final encoder
linearly merges its sorted members with the overlay. Only known successful
creations replace values; failed or indeterminate replacements keep an earlier
successful or initial value. Empty prepared slots are never emitted. Both
counting and output process one bounded pair per admitted turn. Return the
merged createdIds only if the request supplied it, including an empty object.
Failed new reservations remain charged until the batch ends; this conservative
bound also prevents repeated failing methods from growing request memory.

This exception qualifies recording and response emission, not indexed
creation-reference lookup. That later dispatch must either prove its bounds
for this profile or implement the full disk lookup contract before activation.
