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

BodyBytes and BlobCount charge each body once. UploadBytes and QueueBytes
each charge it while their respective lease or submission references exist.
Completed retained submissions still
consume queue count and body categories. Releasing a lease/category does not
release body charge until proven transactional body deletion. Rolled-back
writes may leave allocated database/WAL space, which remains physically charged. Cancel releases only unused reservations; uncertainty cannot
be reconciled as a zero effect. Quota reductions below actual use refuse
configuration rather than deleting data. Effective admission intersects all
applicable quotas; maxSizeUpload does not guarantee available capacity.

The core supplies passive account observations and a cold whole-store
usage_fence. Its consuming initialize_leases path seeds this same ledger
from captured logical/file usage and trusted auxiliary reconciliation.
The trusted coordinator owns ledger uniqueness and its store association.
STORAGE.md owns capture, initialization and quiescence requirements.

### Logical lease implementation

M04c3b1 keeps up to 64 caller-owned cells, with four logical quota pairs per
cell and at most eight cells in one atomic group. Duplicate kinds add across
the whole group; every applicable category must fit used plus pending plus
new charges before any group is installed. Constructor use comes from trusted
store reconciliation and cannot exceed configured caps. These counters are
not disk authority. The helper does not grant filesystem or writer permission;
M08 must couple its reservation to the writer and SQLite commit state.

Group and part tokens validate the complete process-local slot generation.
A reservation inherits the slot issuer's bounded Contended refusal
(RESOURCES.md, M04a2), which reserves nothing. Before admission the
receiving runtime retries it on a later bounded step; a refusal while
publishing a commit fails that delivery job like any other refusal.
Upload reservations and commits, checkpoints and expired-upload cleanup
return it to their caller.
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

### First reserved upload transaction

StoreCoordinator implements this M08 adapter boundary; no listener or
credential verifier invokes it yet. Each device-authorized
upload commits a fresh BlobRow and its device-bound LeaseRow
atomically. The adapter constructs these rows from its actual prepared
input; it accepts neither an arbitrary transaction batch nor caller-
supplied proof of a body digest. HTTP handling, credential verification,
general object mutations and request idempotence remain their own
increments.

The coordinator owns one IndexStore and the single logical ledger seeded
by its consuming usage_fence initialization. Auxiliary owners are
quiescent and pending effects settled during that cold capture. Its
service-facing interface does not expose the mutable persistence core or
another ledger. At least two ledger cells are required: each admitted
job uses one logical cell, and admission leaves one cell for the current
publication's physical effect. Uploads and receiving jobs borrow the
coordinator concurrently and own separate spool inputs. Existing logical
quotas and ingress slots bound the admitted jobs; no connection holds the
coordination mutex while waiting for network input, writing a spool or
preparing its body.

Reserve, replan, commit and checkpoint/expiry maintenance try to acquire
that mutex. Contention returns Busy before effects, except delivery
commit returns its distinct CoordinationBusy variant. Only that variant
promises a retryable prepared delivery under its captured deadline;
policy or storage Busy remains an ordinary failed attempt. Delivery
preparation narrows the job's enforcement deadline to the finalization
cap its trusted caller must capture at dispatch. The ledger reservation,
work meter and spool retain their original receiving bounds; retries
cannot renew either deadline. Expired publication refuses even while
coordination is busy. Once acquired, the mutex covers planning, native
publication and ledger settlement. Delivery plans against the then-
current account snapshot, so an earlier concurrent delivery can supply
its thread anchor. Uploads retain the existing one-time explicit
sequence-conflict replan and refresh that retry's account endpoint under
the commit lock, closing the gap between replan and retry. Their fresh
body/lease rows do not depend on earlier account contents.

Drop/discard retirement and passive usage/status inspection may wait for
the mutex;
the runtime must run those operations and synchronous store work on its
workers, not its network event loop. Drop/discard retires only the job's
own lease. A completed job cannot retire another job
when later dropped. Forgotten jobs retain their quota and spool charge
until recovery without holding up otherwise admissible ingress or
maintenance. A poisoned coordination mutex refuses
future admission and publication; no panic recovery invents a settlement.
Private spool creation and final deletion run outside the coordination
lock. Failed creation cancels the already-reserved lease; known durable
completion remains reportable even if final spool deletion fails.
This is concurrent store coordination, not a runtime fairness claim.

Authentication remains upstream. A trusted authorization provider binds
account, device and upload permission to current policy. Access is
context, not a credential. Reserve checks that policy and binds the
resulting reservation to account/device, coordinator instance, checked
generation, maximum length, original deadline and captured ViewIdentity.
At commit, authorization is checked again under a guard that also
excludes policy publication/revocation until the synchronous transaction
finishes. Guard acquisition has bounded refusal under the original
deadline. A pinned old configuration alone grants no current permission.
The provider must not call back into the coordinator while that guard is
held.

The reservation owns the actual SpoolWriter and then SpoolInput through
exclusive custody; its borrowed staging handle cannot be used
concurrently with commit. Before each write, its wrapper checks
cumulative length against the reservation maximum as well as the spool
ceiling. The coordinator generates a fresh BlobId through admitted
entropy, preserving permanent-ID collision refusal. Input account, ID,
length and digest must match that live reservation. Finishing staging is
preparation only. Discard/abandonment follows IngressSpool cleanup,
including its conservative retired-slot charge when deletion cannot be
proven.

The first upload lease uses LeaseUse::Both for that same account/device.
Its expiry is trusted commit UTC plus 24 hours with checked arithmetic,
matching DESIGN section 5; callers cannot supply arbitrary expiry or
lease rights. The original monotonic deadline remains unchanged across
staging and commit. A stale epoch invalidates the reservation. An early
sequence conflict proven by the core within the same epoch retains the
prepared input and unused reservation; another Conflict code alone
cannot authorize that replan. An explicit bounded replan captures a
fresh identity and revalidates policy before retry, without renewing the
deadline. No old epoch is rewritten to force admission.

Reserve maximum BodyBytes, one BlobCount and maximum UploadBytes before
staging. IngressSpool separately reserves its entire temporary slot.
These are different resources. Existing global category quotas are used;
per-account enforcement must precede advertising the eventual
per-account limits. Before SQL, reserve database/WAL headroom in this
same ledger and pin logical and physical effect tickets. Serialized
publication conservatively reserves all remaining physical headroom.
There is no second quota-used table or public release-used-by-kind
operation.

A core-controlled completion disposition may prove that no
storage-changing work began. Only that proof permits zero physical
effect and release of unused physical headroom without measurement;
Rejected or an Error variant alone is insufficient. After an attempt
that may have changed extents, measure validated database/WAL lengths
under the same exclusive mutation/maintenance ownership, using the
StoreFileUsage validation rules and original operation scope. This does
not repeat the cold whole-account reconciliation scan. A private trusted
ledger transition replaces those measured physical buckets, settling its
physical tickets and pending balances atomically, without disturbing
other kinds or reservations. Rollback may grow extents; checkpoint may
grow the database while shrinking WAL. A proven logical rejection
therefore never supplies a zero physical charge. If the scope has
expired or measurement/reconciliation fails, retain conservative
physical charges and stop mutation admission until reopen and cold
reconciliation, or the eligible maintenance path below.

StoreCoordinator::checkpoint is a separate maintenance operation. It
serializes against publication and settlement, while idle ingress jobs
may remain admitted: they retain no native view or in-flight effect.
The preserved physical-effect cell also admits maintenance when logical
cells are saturated. It takes its own caller-supplied deadline; it
neither renews nor retries an upload. The
operation reserves physical headroom in the same ledger, checkpoints
under the native writer fence and observes validated file extents before
releasing that fence. The original maintenance clock/deadline also covers
observation. A retained native view refuses checkpoint with Busy.

A physical-observation failure can be recovered this way only when the
previous upload or cleanup has a known outcome, logical accounting and
physical tickets were settled (and tickets canceled), and the native writer
remains healthy.
A successful checkpoint with measured reconciliation then restores
admission without changing body or category charges. Any refused
maintenance attempt preserves the prior stop, including a checkpoint
error with successfully measured file extents. Indeterminate commits,
ledger bookkeeping failures and stopped native writers require reopen
and cold reconciliation; this path grants no retry authority over them.
A maintenance observation failure retains conservative physical charges
and requires another separately admitted maintenance operation. Its
receipt reports the checkpoint outcome independently of accounting and
admission state. Runtime scheduling and multi-client fairness remain
unimplemented. Acceptance covers actual WAL reclamation and exact body
reopening, known-success and rolled-back upload timeouts, indeterminate
commit plus unavailable observation, pre-SQL refusal and measured
checkpoint error preserving a prior stop, native stopped-writer refusal,
and maintenance timeout followed by successful reconciliation.

StoreCoordinator::expire_next_upload is explicit trusted maintenance, not
an authenticated client operation. It uses the same publication lock
and may run beside ingress; removing proven used charges preserves other
jobs' pending reservations. Each call visits at most one LeaseRow
in account-local BlobId order using the existing primary key. None ends
a pass; an Ok disposition permits advancing to that ID. The caller resets
the cursor after the end so later passes include newly inserted lower
IDs. Retained means this call did not remove the lease. Unexpired leases
do not obstruct progress to subsequent IDs. Scheduling remains separate.

Trusted UTC is sampled before planning and again before mutation. A lease
is expired when expires_at is less than or equal to that UTC millisecond. An
expired lease is deleted atomically with its body only when the same
snapshot finds no email or submission owner; completed submissions still
own their transmission bodies. The expected sequence and epoch bind that
plan to the commit. Permanent ID retirement remains in force. Known
success releases UploadBytes and, only for a removed body, BodyBytes and
one BlobCount. It does not release queue charges. Rejection releases no
logical usage; an indeterminate result retains charges and stops admission
until reopen. Private typed retirement transitions check underflow before
SQL and settle after known success; there is no public release-used API.
Physical reservation and observation use the upload's existing single
ledger path. A known cleanup outcome with settled accounting and healthy
writer can recover a failed file observation through separate checkpoint
maintenance. Its receipt preserves the durable outcome even when
accounting stops further work. Removing a body frees reusable SQLite pages;
it does not assert that the database file shrank.

Reserve typed outcome storage before effects. Known COMMIT success
retains its durable receipt and exact logical increments (length, one,
length), even when later physical accounting or spool cleanup fails. The
result separately reports whether admission is stopped; an accounting
error cannot relabel that receipt as rejection or indeterminate COMMIT.
Proven rejection adds no logical body/category usage. Indeterminate
COMMIT conservatively charges planned logical effects, stops admission
and forbids duplicate retry until recovery. Completion and unused-charge
release may run after deadline; expiry cannot undo a durable result.

Acceptance uses real ingress and native commits: exact body/lease
reopening from nonempty reconciled usage; reservation/source ownership
and stale identity refusals; policy revocation between reserve and
commit; unchanged deadlines and quota/slot refusal before effects;
sequence-conflict replan; logical rejection with measured physical
usage; expiry between the coordinator precheck and native admission
leaving admission usable; and known durable versus
indeterminate/accounting-failed completion through existing fault seams.
Tests answer these behavior and ownership questions. No new byte-exact
memory layout or resource permutation matrix is a prerequisite.

### Reserved SMTP delivery

StoreCoordinator owns the native writer and single ledger for uploads and
receiving. Delivery captures the validated session's DATA envelope, reserves
maximum stored BodyBytes and one BlobCount before 354, and owns one ingress
slot. It consumes no UploadBytes. Receiving and upload jobs can remain
admitted concurrently under the shared coordination rules above. The
direct receiving runtime now supplies bounded scheduling and socket/TLS
facts. Gateway and device credential verification remain separate consumers.

A connection-specific DeliveryAuthorization supplies actual peer/TLS/gateway
facts and receiving permission. Its commit guard rechecks current policy and
excludes relevant revocation through publication. Access and receipt values
are passive context, never credentials. Recipient routing stays pinned.
The guard may accept benign generation changes; it must refuse revoked
network/gateway authority. The runtime still owns the implementation binding
those facts to its actual connection and serialized policy publication.

The header buffer is limited to configured root headers plus one decoded
line. The shared scanner decides the raw boundary; the adapter removes only
recognized Return-Path fields, prepends truthful trace fields, and streams
the remaining body into the same spool. It preserves other raw bytes,
including malformed header/body boundaries. An orphan leading continuation
gets an explicit separator so it cannot join the generated Received field.
Both incoming recognized headers
and retained headers plus local trace obey header_bytes. A size refusal is
HeaderLimit, mapped to SMTP 552 5.3.4; work, clock and I/O errors remain
separate temporary refusals. No message-sized buffer or rendering is needed.
Trace dates use trusted ingress-start UTC through the shared td-civil
conversion. The format-derived allowance bounds generated bytes and is
excluded from incoming SIZE; the full stored ceiling is reserved. The actual
local trace must itself fit the header ceiling before DATA admission.

Thread selection retains exact source ranges for POLICY section 5's bounded
lookback, validates whole fields before using candidates, and replays each
selected identifier alone with the original work meter and deadline. A field
that exceeds the fixed comment-nesting grammar is skipped as invalid; work
and interpretation-budget exhaustion still refuse creation. Raw writes charge
bytes, not one object record per physical line. Large folded fields are
written in spool-sized pieces under that same deadline. The
commit snapshot requires exactly one Inbox and resolves live anchors before
inserting the new email's own anchor. Inbox admission is also checked before
354. Missing/duplicate Inbox, index error or budget exhaustion refuses rather
than inventing a folder or silently choosing another thread.

One native transaction publishes the body, SMTP receipt, email, immutable
thread, Inbox membership, optional anchor and Email/Thread/Mailbox history.
The writer compares captured epoch and sequence. Upload and delivery share
logical/physical effect settlement: known success survives ancillary
accounting/cleanup failure; uncertainty retains charges and stops admission
until recovery, with no final SMTP acceptance. Abandonment retires ingress
and unused reservations. Native transcript fixtures verify reopen after 250,
alias deduplication, raw trace filtering, threading, quota/size/work refusal,
revocation and known versus uncertain publication. They do not activate a
listener or establish whole-service crash/fairness qualification.

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
under the original queued admission cap.

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

### Private response-file custody

RequestSpool and StoreCoordinator::retain_response implement file custody
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
across 64. ResourcePlan still validates its current 128 MiB planning budget;
DESIGN section 5 separates that arithmetic from process memory claims.
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

### Device upload publication guard

`http_auth::DevicePolicy` implements UploadAuthorization for the live private
device registry. The caller supplies a device ID established by successful
HTTPS authentication; knowing or passing an ID is not authentication. The
policy checks the registry's account, healthy state and current revocation
state under its mutex. A successful guard retains that mutex through the
coordinator's commit, excluding create/revoke publication until it drops.
The guard contains only fixed Access metadata and the registry lock, never
a password. The existing coordinator checks its account/device/generation
and reacquires the guard before a prepared upload can commit or replan.

Admission checks the real clock before and after lookup, refuses expiration
or regression, and uses try_lock so contention returns Busy rather than
waiting past a deadline. Poison or retired registry state refuses as
WriterStopped. The configuration generation is immutable for this policy's
lifetime; future configuration replacement must share the same publication
exclusion. This adapter is not yet wired into HTTPS upload serving and does
not activate a route or expand mail credentials into OS authority.

The mutex serializes all registry access, including other upload guards and
HTTPS authentication, for the bounded publication interval. Runtime callers
must retry Busy within their original deadlines or return a transient refusal;
they must never replace admission with an unbounded mutex wait. A panic while
publication holds the guard poisons the shared registry and deliberately
requires service restart, even if the registry itself was only read.

### HTTPS upload worker

The mailbox-access profile routes authenticated POST requests at
`/jmap/upload/{accountId}` to one dedicated synchronous worker. The account
must match the configured account. A single permit lasts through final
response flush or connection retirement; competing uploads receive 429.
The worker retains its client slot while storage work runs, and the network
loop uses try_lock to keep unrelated HTTPS connections progressing.

Content-Length and chunked bodies pass through BodyReader into a reserved
Upload. The decoded ceiling is message_bytes (or the smaller declared length);
total wire input is bounded by eight times that ceiling plus 64 KiB. Two 16 KiB
worker buffers suffice; neither the complete upload nor its body is retained
in memory. Header credentials are retired before the transfer. Content type
is limited to 1024 ASCII header bytes and serialized with the shared JSON
string frame. Missing Content-Type uses application/octet-stream.

Expect: 100-continue is acknowledged only after body bounds and storage quota
admission. The body phase has an absolute transfer deadline and idle timeout,
both capped by the connection lifetime. Publication has its enclosing exchange
deadline through receipt flushing, rechecks the device, and allows the
coordinator's single sequence replan. The JSON success receipt uses only a known durable outcome and includes
accountId, blobId, type and size. A stopped ledger or cleanup error after a
known commit attempts that receipt and a bounded TLS close before retiring.
Any intervening clock, connection or encoding failure still retires the
service; a client cannot prevent retirement by refusing the receipt.
An indeterminate outcome never produces success.

Shutdown stops body transfer, joins the upload worker and releases incomplete
spool/quota owners. A commit already underway settles before the worker joins;
a lost connection or expired exchange can leave a durable upload without an
observed receipt. The fixed connection cap is one hour; raised transfer limits
remain subject to that cap. Body progress does not extend the absolute limit.
The worker wakes on admission and polls nonblocking TLS progress at 5 ms.
Upload quota pressure reclaims committed unreferenced uploads oldest first
within the configured account, with BlobId breaking equal creation dates.
Only BodyBytes, BlobCount and UploadBytes pressure initiates reclamation;
contention, physical storage limits and other failures do not. Every deletion
rechecks device authorization and holds its guard through atomic deletion
and ledger settlement. Admission rechecks current quota before choosing a victim,
never reclaims pending ingress, and stops once a fresh reservation fits.
No available unreferenced victim leaves the original quota refusal. A stopped
or uncertain cleanup retires admission rather than proceeding to a new body.
All retries retain the original deadline; native selection also retains its
existing VM/heap bounds. Discovery, API, Blob/copy and EventSource activation
remain prerequisites to advertising Core.

Before each pressure retirement, an aggregate over the same eligible rows
must cover every current shortfall; otherwise that step deletes no victim. The aggregate
and oldest-row selection share the native scope and existing email/submission
blob indexes. Reservation and reclamation share one monotonic watermark.
A single no-progress retry covers quota released by concurrent work without
allowing an unchanged fits/refusal disagreement to spin.

An admission retires at most 64 victims before returning transient Busy if
reservation still fails. Each scan has the native VM bound and all attempts
share the original deadline. Earlier committed evictions remain after later
quota consumption, refusal or operational failure. Reclamation uses the
Content-Length declared for a fixed-length request. A chunked request reserves
its maximum without eviction and receives the quota refusal if it cannot fit;
its unknown actual size cannot justify premature deletion of recent uploads.
Referenced bodies retain any upload-lease charge until ordinary expiry;
future reference-creating methods must account for that retained charge.

Authenticated GET downloads share the fixed HTTPS body worker and its single
permit with uploads. A busy body worker returns 429 before storage lookup;
the permit lasts through body flush or connection retirement. The worker
verifies the complete raw blob under authorized snapshot ownership before
sending headers, and checks cancellation and the original exchange deadline
between output steps. Verification remains bounded by the storage deadline.
Response idle time starts after verification; output progress never extends
the absolute exchange or one-hour connection cap. Every request rehashes the
whole body; no range or verification-cache shortcut is claimed. After
verification, the output deadline narrows to the actual body length using
the original operation start, never renewing elapsed time. An admitted read
can finish after device revocation under its existing bounded snapshot.

The configured download template percent-decodes each component once within
fixed name/media bounds; plus stays literal. Names never become paths and
are UTF-8 percent-encoded in Content-Disposition. Content-Type rejects
non-ASCII and control bytes before header output. A verified blob is streamed
through one 16 KiB buffer, with Content-Length and attachment disposition.
Responses use no-store and nosniff. No body or credential enters a log.
Before output, inaccessible blobs return 404; storage failures, including
verification corruption, return 503 without exposing provisional bytes. After output begins, errors close the connection without a
second HTTP response. Shutdown cancels streaming and joins bounded storage
work already running. Successful downloads close without unread-data drain.

Authenticated POST /jmap/api uses the same fixed body worker as upload and
raw download. Its sole API permit is distinguishable from a binary transfer:
a competing API gets the maxConcurrentRequests problem with HTTP 429, while
a binary transfer occupying the worker gives temporary HTTP 503. Conversely,
a binary request refused behind an API receives 503, not an upload-limit
refusal. The permit lasts through response flush, including error output.
The prepared Session records one API request and one upload; authenticated
discovery advertises Core with these enforced upper bounds. Content-Type
must name application/json.

Before 100 Continue or method execution, the API reserves the complete response
cap and allocates the resource plan's byte/token arenas. It decodes bounded
Content-Length or chunked framing into the byte arena, then performs incremental
I-JSON parsing, whole-envelope validation and mandatory-response preflight.
Each preparation turn checks cancellation, the original exchange deadline and
the retention work meter; preparation also has the execution phase deadline.
Device authority is fresh after body receipt and again before dispatch.
Revocation at these checks returns 403, as for uploads; initial invalid
credentials still receive 401. Core
methods are currently side-effect free; future mutators need their own guarded
publication boundary. The account always comes from trusted configuration.

The complete response is retained before final headers. The existing wire
encoder counts the exact envelope, then streams it with Content-Length through
fixed scratch under the original exchange and response-idle bounds. All
response files and charges remain owned through transmission and cleanup.
Uncertain ledger settlement retires admission, including after known output;
no cleanup failure fabricates a fresh response. A failure after a write attempt
closes the connection. A fully flushed 100 Continue permits one final response.
Request syntax/envelope/capability and advertised size/call-limit failures use
RFC 8620 problem details; internal parser/work/storage ceilings remain resource
failures rather than being misreported as invalid JSON. Shutdown cancels reads
and output and joins already bounded storage/dispatch work.

Authenticated discovery and EventSource run on the fixed network reactor.
Discovery copies the prepared bounded Session only after retiring the request
head, so credentials and the largest Session response do not coexist in the
per-connection scratch budget. Absolute-form targets use their URI authority
for same-origin admission; Host remains syntactically required.

EventSource owns one fixed coalesced frame and no store view, response spool
or body-worker permit. A separate event_streams counter is bounded by the
HTTPS connection pool, and the per-peer cap still applies. Startup refuses
Core activation with zero event capacity. The backend snapshot operation is
nonblocking and only tries the device registry; every turn rechecks device
access before output. The td-mail Mail profile does not yet connect account
state to this endpoint: it emits requested pings only. td-mail uses requests
instead. The existing snapshot encoder handles future StateChange filtering,
reconnect and close-after-state behavior.

The HTTP stream is close-delimited text/event-stream. Headers and each frame
must flush within one original event_stall_seconds window; partial writes do
not renew it. Acknowledgement follows complete flush, with coalescing only
between frames. A silent stream has no response-idle timeout, but retains its
absolute connection deadline and detects peer EOF. No request body or pipeline
is accepted. Disconnect, revocation and shutdown retire the stream and its
slot without retained database or temporary-file ownership.

### Retained sort files

StoreCoordinator::retain_sort reserves a caller-selected nonzero file cap
against the existing SortBytes bucket, before creating a private file.
A single cap above configured sort_disk_bytes is a deterministic Capacity
refusal; contention or aggregate reservations can still refuse admission.
All concurrent runs, merge outputs and other sort scratch must hold these
reservations simultaneously. There is no uncharged intermediate-file path.

SortRetention wraps the same file/effect custody used by response retention.
It shares RequestSpool's 32 slots and closed namespace. Sort reservations
use only the first 24 slots, leaving eight unavailable to sorts for response
admission; existing responses in the first 24 may reduce sort availability.
The internal filename remains response for both purposes, and cold cleanup
discards either kind. The filename alone does not identify a file's purpose.
Its API supports bounded append, sealing a readable prefix, exact reads and
resetting a file for a later merge pass. Reset retains the original quota,
deadline and work meter. It invalidates prior run offsets; the sorter must
stop using them before reset. It does not publish a client response.
Cleanup proves removal before releasing SortBytes. Uncertain cleanup charges
the entire file cap as used in SortBytes and retires the slot; it never
refunds unrelated ResponseBytes. The existing owner failure and recovery
rules apply unchanged.

This boundary supplies file custody, not an external-sort implementation or
whole-job work admission. A multi-file sorter must additionally retain one
cumulative job meter and original execution deadline, enforce at most eight
merge inputs and 32 total open files, bound resident records, and freshly
validate the retained read view before publishing query results. Each new
file has its own foreground work meter, which is not a job budget; the
cumulative caller-owned job meter must cover every file and reset. No query
method is enabled here.

The per-file admission cap and SortBytes quota both derive from
Plan::sort_bytes, the validated resources sort_disk_bytes value. This is
currently the only runtime file-creation path that charges SortBytes;
AuxiliaryUsage may initialize retained usage during quiescent recovery.
Do not reserve a second SortBytes charge for the same files in a future job
owner. An uncertain cleanup of a file holding the entire sort cap can block
all further sort admission until quiescent cold recovery; the conservative
full-cap charge is intentional.

## Native sort-job custody

StoreCoordinator::sort_job couples two SortBytes reservations with one
coordinator-wide resident workspace permit. A workspace admits at most 256
records of at most 2048 bytes and eight merge heads, within the existing
1 MiB sort-buffer reservation. It frees both vectors before releasing the
permit. A contending workspace receives Busy; dropping the workspace does
not release file reservations, which remain with NativeSortJob. The service
uses its one shared coordinator; creating separate coordinators is not a
way to multiply the service reservation.

Each file cap is positive and at most half the configured aggregate sort
capacity, so the pair's requested reservation fits before contention is
considered. Both reservations remain held across every reset and merge pass.
If creation of either file fails, all acquired file/workspace custody unwinds;
uncertain file cleanup still follows the existing conservative SortBytes
settlement. File cleanup does not require the execution deadline to remain
open.

NativeSortJob retains one original foreground work meter and deadline for
all engine polls, both files, resets, output reads and source callbacks.
It conservatively charges poll admission and native operations separately;
source/comparator lookups additionally charge through SortWork. No file or
pass replaces this meter. Every charge checks time before and after work.
A source callback that ignores a failed charge still fails its enclosing
operation; all source, I/O, budget and clock failures are sticky for the job.
Each source callback must yield within the existing coordinator-turn bounds.
The adapter contract is trusted, not a sandbox for arbitrary callbacks.

The source retains its authenticated account/view binding and must freshly
validate it before response publication. Sorted-file completion and bounded
output reads do not establish that permission. This boundary activates no
query method and does not claim final client-read conformance.

This is a per-job meter created at construction, not the already consumed
request/argument-admission meter. Construction charges one control record.
Query activation must create at most one job for that method and propagate
exhaustion without constructing a replacement job. Filtering/decoding during
sorting must use the supplied meter, rather than a newly created one. This
boundary alone does not establish an aggregate budget for arbitrary chains
of independent jobs or other query stages.

SortWork enforces cumulative declared source-callback charges of at most
64 KiB and 256 records, resetting those two step counters for each callback
without resetting the job meter. Byte-only charges are valid; a wholly empty
charge is invalid. Unmetered callback CPU remains a trusted-code obligation.
An already admitted job also checks writer-stop state before work. Invalid
output indices are terminal caller errors, not end-of-stream markers; callers
iterate only within the returned count. Busy propagates as temporary pressure;
there is no internal retry queue or replacement job after execution failure.
A forgotten workspace fails closed by retaining its permit. The generic
algorithm's raw slices remain a trusted primitive; production callers use this
native admission path. A compile-time bound pins maximum workspace buffers
and fixed swap/metadata allowance below the existing 1 MiB reservation.

Mailbox/query uses the native single-workspace permit for both sorting and
its bounded filter workspace. A compile-time bound combines 256 resident
513-byte records, eight merge heads, 64 maximum filter frames, 64 decoded
4096-byte distinct operands, one pending operand and fixed decoding/row/stack allowance below the existing
1 MiB reservation. Frame storage is reused across rows; decoded distinct
operands exist only during pre-scan validation. Completion drops the filter
storage before releasing the sort workspace, so output streaming cannot
retain uncharged filter memory after a later query acquires the permit.

Every query uses one sort job, with no reconstruction after failure. Source
steps conservatively precharge retained scanner reads, bounded decoded-text
work and native lookups; actual retained reads also debit the original request
retention meter. Sorting control and backend operations retain the adapter's
conservative double charges. A disk cap is not a promise that every query can
fill it before its CPU/I/O/deadline ceiling. Deterministic job work exhaustion
is storageWorkLimit; elapsed deadline and shared workspace/file/view pressure
are temporary serverUnavailable. Neither publishes partial IDs or totals.

## Device read/star response admission

The active Email/set subset accepts only update patches for `keywords/$seen`
and `keywords/$flagged`, with true to set and null to remove. Unsupported
properties or false values refuse that object as invalidProperties; absent
Email IDs report notFound. Nonempty create/destroy are not implemented.
ifInState is checked against the account epoch and sequence under coordination.
The batch ceiling is the smaller of objects_per_method and 256, also advertised
as maxObjectsInSet. Device activity is checked again under the registry guard
held through COMMIT, independently of the initial HTTP authentication.

Before store effects, the dispatcher allocates its complete bounded result,
materializes that bound in the unpublished response tail, and prepays its final
spool overwrite from the original request meter. The store plans and commits
all valid existing objects atomically. The final overwrite consumes that credit
without a new execution-phase test or work grant; it still observes the original
outer deadline and physical spool failures. Such terminal failures close the
exchange for reconciliation. Known commits never become serverFail or
serverUnavailable. An indeterminate COMMIT reports serverPartialFail when
response retention remains usable. Earlier method results remain retained.
