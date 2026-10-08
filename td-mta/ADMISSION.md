# Disk admission, bounded work and request retention

This is the normative companion to RESOURCES.md, API.md and STORAGE.md.
admission.rs validates an immutable disk/work plan without allocating pools
or inspecting free space. Fixed logical leases and effect tickets provide
coordinator accounting; SQLite owns actual body/metadata transaction/recovery state.
The low-level IndexStore does not implement the service reservation coordinator.

## 1. Disk accounting

Lengths, offsets, quotas and arithmetic use checked u64. Only bounded I/O
chunks convert to usize. One future coordinator serializes used and pending
logical charges, including database/WAL growth, scratch and logs. Logical quotas do
not promise successful I/O or reserve filesystem blocks.

| Resource | Default ceiling |
| --- | ---: |
| Logical stored body bytes | 4 GiB |
| Stored blob count | 250000 |
| SQLite main database, bodies and metadata | 8 GiB |
| SQLite WAL | 17280796224 bytes |
| Upload category | 128 MiB |
| Disposable ingress files | 512 MiB, 16 full-message slots |
| Queue category | 256 MiB, 1000 retained submissions |
| Sort scratch | 64 MiB |
| Request retention | 128 MiB/request, 256 MiB aggregate |
| Disposable caches | 128 MiB |
| Logs | Five 8 MiB files |
| Mutable cold state | 16 MiB |

DiskLimits caps logical body bytes at 4 GiB and blobs at 1000000. The combined
database ceiling is 8 GiB and WAL logical admission may be at most 32 GiB.
Responses remain at most 1 GiB/request and 4 GiB aggregate, caches 1 GiB and
cold state 64 MiB. database_bytes must equal the fixed 8 GiB native ceiling;
wal_bytes must cover the conservative fixed 17280796224-byte core bound.
These values do not reserve filesystem space. SQLite may refuse earlier on
page, native heap, deadline or I/O exhaustion. The body quota is independent
of relational/page overhead; physical database usage includes both. Logical
reductions below actual usage refuse. Service coordination still owns logical
body, category and response admission before invoking the core.
Raw quota must cover message_bytes, aggregate response quota must cover its
per-request quota, and per-request response quota must fit
32 * json_bytes + 4096 * json_methods + 64 KiB. OnlineBackground requires at
least two storage views; ForegroundOnly requires one. The cache must fit the
1 KiB maintenance cursor reserve. These relationships do not enlarge RAM.

Upload/queue are subquotas of raw bodies: one body is charged logically once,
even when both categories reference it. Completed retained submissions still
consume queue count and body categories. Releasing a lease/category does not
release body charge until proven transactional body deletion. Rolled-back
writes may leave allocated database/WAL space, which remains physically charged. Cancel releases only unused reservations; uncertainty cannot
be reconciled as a zero effect. Quota reductions below actual use refuse
configuration rather than deleting data. Effective admission intersects all
applicable quotas; maxSizeUpload does not guarantee available capacity.

The core supplies passive account observations and a cold whole-store
usage_fence. STORAGE.md owns their counting and reconciliation limits.

### Logical lease implementation

M04c3b1 keeps up to 64 caller-owned cells, with four logical quota pairs per
cell and at most eight cells in one atomic group. Duplicate kinds add across
the whole group; every applicable category must fit used plus pending plus
new charges before any group is installed. Constructor use comes from trusted
store reconciliation and cannot exceed configured caps. These counters are
not disk authority. The helper does not grant filesystem or writer permission;
M08 must couple its reservation to the writer and SQLite commit state.

Group and part tokens validate the complete process-local slot generation.
A bounded extension increases a part's reservation without allocating another
cell; writer state and all logical caps still apply.
Initially zero amounts disable their positions for the lease lifetime; an
extension cannot activate them. An enabled position consumed to zero keeps
its kind and can be extended. Packed kinds and amounts avoid pair padding. A
linear effect ticket pins a part before work. A busy part cannot be extended,
reused or canceled. Proven completion consumes only its exact bounded charges;
an uncertain effect conservatively consumes the planned charges. Invalid
completion leaves the ticket and reservation pinned. Completion can run after
the lease deadline; expiration refuses new work and cannot undo effects.

Streaming body bytes and publishing references belong to one SQLite
transaction. A known rollback proves no logical body was added, but does not
prove zero physical database/WAL growth. Cancel releases only unused
reservations; used physical charges require measured reconciliation. The logical helper has no public operation to release used
charges. M05/M08's object ledger and proven cleanup/commit transitions own
that later integration; a freed lease token cannot authenticate object cleanup.
The writer and future runtime coordinator wrap this same logical ledger,
without duplicate quota-used counters. Typed SQLite commit/checkpoint and object cleanup transitions must
account for all recycled buckets, including scratch and logs. Proven effect
amounts are trusted adapter inputs, not capabilities against arbitrary code.
Runtime integration must restrict who can supply that proof.

The coordinator's job registry retains every lease and effect ticket until
completion. Both handles carry must-use diagnostics. Bounded expiry enumeration
returns root tokens, including busy groups; cancellation revalidates them and
refuses a group with any busy member. Expiry never establishes worker quiescence.
Losing a ticket leaves its part pinned: only a worker-quiescence fence plus
trusted effect/object reconciliation can support recovery. Until that path is
implemented, stop admission and restart/reconcile under the store lock rather
than inventing a zero-effect completion. Dropping the table leaves occupied
cells, preventing accidental reuse by a new table. The backing-memory owner
can reset cells; this guard is not an integrity boundary against that owner.
Unexpected internal failures after slot acquisition poison the table and stop
new work; ordinary refusal rolls back without changing live charges.
No syscall, publication or authoritative cleanup is implemented here.

## 2. Reservations and I/O failure

Reserve logical bodies/count, combined database/WAL, response and category growth before taking
responsibility. An account/size/deadline lease grants only its named operation.
Before starting a SQL write the core reserves worst-case WAL room; Busy asks
for bounded maintenance rather than allowing growth past the ceiling.
BlobSource supplies provisional bytes, not a durable publication proof or
logical quota ticket. Service callers must couple reservations to the actual
SQLite result before mutation admission is activated.

Known rejection before COMMIT requires rollback. Deferred constraint or busy
refusal with an intact transaction also rejects only after rollback. A proven
successful COMMIT reports its durable result even if its deadline expired
during completion. Other COMMIT errors remain indeterminate and stop writes
until reopen/recovery; new snapshots remain available. Preserve actual
database/WAL charges on both outcomes. Native memory/page exhaustion, I/O,
SQL integrity and body digest corruption remain distinct failure domains.

Checkpoint refuses while views are held; transactional body deletion preserves
old body snapshots through SQLite. Bound the wait
within the enclosing deadline; a timeout returns Busy/error, never a claim of
successful cleanup. Hold the writer fence only for the actual exclusive step;
a scheduler must establish quiescence without canceling durable effects.
Whole-service recovery and quota reconciliation remain
unimplemented; do not infer them from pure accounting helper tests.

## 3. Work budgets and deadlines

WorkLimits retains finite job budgets: foreground 120s/8 GiB/2000000 records;
changes 30s/128 MiB/1000000 records; request 300s; commit 30s/256 MiB/250000
records; checkpoint 60s scheduling target/up to 8 GiB native page writes; GC drain 30s, exclusive 120s/8 GiB/128000000
records/1000 blob removals; backup 900s/16 GiB; admission wait 1s. Raised values
are at most 16 times defaults. These are future coordinator job ceilings,
not evidence that full jobs or native resource limits have been qualified.

The core retains one deadline and 8000000 SQLite VM steps per view or commit.
Cold schema/header validation uses that same fixed work limit under its startup
deadline. Per-instruction callbacks sample the original monotonic clock.
Queries check before and after work. Clock and VM-fuel failures are sticky;
retries
or chunk boundaries cannot renew work. Rollback cleanup bypasses expired
request fuel without clearing its failure. Writer acquisition and SQLite busy
wait both refuse immediately with Busy; callers schedule bounded retries. Body digest I/O
checks the same clock between bounded chunks. Complete service work meters
must additionally charge bytes/records, scheduling turns and response output.

### Network timers

Both idle timers and absolute operation deadlines are enforced. Progress resets
only the applicable idle timer. Main's scheduling wait remains at most five
milliseconds, not a five-millisecond network timeout.

| Operation | Default timing policy |
| --- | --- |
| TLS handshake | 10 seconds total |
| Configured-name DNS lookup | 5 seconds total across UDP/TCP/CNAME work |
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
not metadata records or backups, and do not need fsync. Charge every byte before
writing. Cleanup after completion/disconnect; abandoned files are discarded
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
exact escaped call IDs, one bounded method-error result for every allowed call,
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

Known committed creations enter createdIds even if their enclosing method
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

### References and creation IDs

Implement RFC 8620 section 3.7 against immutable retained response bytes:

1. Find the first earlier response with the exact resultOf call ID.
2. Check that response's name against name; do not search later same-ID replies
   for a more convenient match (implicit responses share a call ID).
3. Apply JSON Pointer to its arguments, including ~0/~1 escaping, array-index
   rules and the RFC's array-only wildcard mapping/order/flattening behavior.

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
Use indexed lookup and a bounded append overlay, not a full linear map scan
per reference; prove the largest admitted creation chain fits its work budget.
Return createdIds only when it was present in the request, including its input
mappings and new mappings. A failure/cancellation does not manufacture an ID
for an uncommitted object. Creation references and result references are
separate mechanisms and both need exact retention.

## 5. Required evidence

Before activation, qualify native heap/stack/RSS with combined owners; full
logical quota reconciliation; exact result retention; metadata page/WAL
capacity refusal; injected COMMIT/rollback/IO faults and reopen recovery;
atomic body/metadata writes and transactional deletion; snapshot/pool exhaustion and bounded
maintenance progress. Record independent failure domains and verified red
controls. Pure Rust/MIME zero-allocation evidence excludes SQLite native
allocation and does not establish whole-service admission or durability.

## Retained scheduling and quota requirements

A coordinator turn processes at most 64 KiB or 256 records before yielding.
External sort merges at most eight runs and owns at most 32 open files.
A deletion with no physical growth remains admissible when a logical quota
is full; physical WAL capacity still applies. Full queue quotas refuse new
submissions before publication. One long background-view permit preserves
a foreground view when storage_views is two; backups and outbound transfer
share it. These scheduler/coordinator rules remain requirements, not active
workers in the storage core.

M09 owns bounded network/DNS fault fixtures; M11 request retention and lost
response; M13 query/sort fan-in and fairness; M17 phase/result reservations
and queue saturation; M20 restore/offline consistency; M21 backup view and
body lifecycle; M23 steady-state resource/concurrency qualification.

## Disposable ingress reservation pool

The concrete IngressSpool reserves an entire configured message_bytes slot
before create_new, using smtp_sessions + https_connections cold slots. The
default is 512 MiB across 16 slots; the theoretical hard ceiling is 2 GiB
across 64. ResourcePlan still validates the complete 128 MiB memory budget.
This pool has no hot used-byte counter: partial and prepared files keep the
full reservation until their descriptor closes and owned inode unlinks.
Failed cleanup retires the charged slot until bounded startup cleanup. The
separate ingress-only root excludes every authoritative database path.

This is temporary disk admission, not a durable body/upload/queue grant.
Future protocol coordination must account for this pool alongside durable
body/category and physical database/WAL limits, authenticate metadata and
couple acknowledgements to proven SQLite COMMIT. The existing logical effect
ledger and ports::Reservation/BlobWriter contracts remain separate; the
standalone primitive does not manufacture coordinator reservation identities.
