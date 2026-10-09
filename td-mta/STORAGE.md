# td-mta storage

This is the normative physical storage contract. [FORMAT.md](FORMAT.md)
owns transient application key and row encodings; SQLite owns relational
storage, pages, WAL, locking, transactions and crash recovery. [API.md](API.md)
owns the typed adapter boundary and [QUEUE.md](QUEUE.md) submission policy.
The storage core is implemented; protocol handlers, mutation authorization,
service admission, administration and deployment remain separate.

## 1. Authority and layout

One locked root contains one SQLite database for all accounts:

```text
metadata.sqlite3                 authoritative bodies, metadata and indexes
metadata.sqlite3-wal             SQLite write-ahead log
metadata.sqlite3-shm             SQLite coordination state
LOCK                            cooperative process writer lock
```

Complete immutable message, upload and transmitted-copy bytes live in the
`blob_chunks.body` rows. Relational columns store mailbox names, membership,
keywords, object IDs, lengths, SHA-256 digests, threading, SMTP envelopes,
submissions, recipients, leases, import mappings and retained changes.
No generic binary record table or generic reference table is persisted.
Parsed-body/search caches may be rebuilt; bodies and metadata are authoritative.
There is no permanent body-file tree, custom journal, replay map or selector.

The safe std adapter requires an operator-controlled stable namespace,
caller-owned private directories, regular private files with exactly one
hard link and a retained cooperative LOCK. It does not defend against
another process with the same filesystem authority replacing paths.
SQLite uses its bundled Unix VFS; td-owned Rust adds no direct syscall
or unsafe allowance. A database or sidecar symlink, wrong owner/mode,
extra hard link or oversized file refuses startup. Creation refuses
preexisting database/WAL/SHM paths. Application ID, exact schema version
2, closed schema and 4096-byte pages are checked before accepting the
store. Full integrity_check, foreign_key_check, per-Email anchor
cardinality and complete blob chunk geometry are explicit
validate_integrity maintenance, not an opening scan. SQLite validates
physical pages on access and body pins verify their full digest. Earlier
formats are refused; no automatic migration or overwrite is provided.

A separate ingress-only locked root holds LOCK and slot-00 through slot-63
for disposable prepared bytes. It must never share the authoritative database
root; the disposable ingress section below owns its cleanup contract.

## 2. SQLite and resource policy

The private dependency is rusqlite 0.40.2 with bundled SQLite 3.53.2 and
hooks and limits features. Exact manifests, locks, active features and
archive checksums are pinned by builder/src/crypto_policy.rs. No system SQLite, extension, attachment,
external SQL or ambient pkg-config selection is accepted. Runtime statements
are closed, parameterized queries. Public interfaces expose typed rows, keys,
body streams, changes and errors, never native connections or caller SQL.

Use 4096-byte pages, at most 2097152 pages (8 GiB), WAL, synchronous FULL,
foreign_keys ON, defensive mode, trusted_schema OFF, in-memory temporary
storage, mmap disabled, cache spill enabled and zero busy timeout. Each
connection requests a 128 KiB page-cache target, not a hard ceiling. Spilling
permits a maximum body write without retaining its dirty pages in RAM.
Compile-time limits cap SQLite's shared requested heap at 16 MiB and each
allocation at 9 MiB. These limits do not measure allocator overhead or RSS.
SQL values are bounded to 69632 bytes (64 KiB chunks plus row headroom); SQL text is at most 8192 bytes. Application batches contain at most
4096 operations and 1 MiB transient encoded metadata, with 1024-byte keys and
65536-byte row values. Body content never enters these metadata buffers.

The complete encoded message or upload has a hard 32 MiB ceiling. Configured
message_bytes may reduce it. Body writes and reads use chunks of at most
64 KiB, with fixed caller/writer scratch and indexed chunk rows. At most two
chunk reads serve an unaligned 64 KiB request; no handle repeatedly walks a
message-sized overflow chain. Chunk insertion and final durable commit are
synchronous native operations: checks before and after them do not promise a
scheduling yield inside each page allocation or fsync. A supplied Read implementation must
itself obey its admitted I/O deadline; a synchronous call cannot forcibly
interrupt an arbitrary reader.

Each write reserves worst-case WAL room for a whole database's changed pages,
plus 32 extra frames for commit and sector padding, plus the WAL header:
`(2097152 + 32) * (4096 + 24) + 32` bytes. The WAL ceiling is twice that
bound, 17280796224 bytes. This conservative disk bound is
not preallocation or a free-space guarantee. The WAL index can map up to
34 MiB outside SQLite's 16 MiB heap, explicitly charged in the startup ledger.
Crash recovery can reread the entire WAL; checkpoint can write up to 8 GiB.
These native calls cannot guarantee a yield at a caller deadline. Return Busy
before writing when the existing WAL leaves insufficient room. SQLite reuses
already-spilled frames of the current transaction. journal_size_limit is not
used as a live hard ceiling. Explicit TRUNCATE checkpoint requires no live
views; automatic WAL-size checkpoints are disabled. Closing the last native
connection can still run SQLite's passive checkpoint and remove the WAL.
That synchronous drop-time work has no application deadline and may copy up
to the 8 GiB database ceiling; explicit maintenance before teardown avoids
leaving that work to connection destruction.

The 9 MiB individual allocation ceiling accommodates SQLite 3.53.2's
contiguous checkpoint iterator. At the admitted WAL ceiling there can be
4194368 frames and 1025 index segments. On x86-64 its request is
`8 + 1025*32 + 4194368*2 + 4096*2 = 8429736` bytes, including merge scratch;
it is already eight-byte aligned, and SQLite's fallback allocator adds an
eight-byte C header. The shared 16 MiB heap cap still applies to this buffer
and every retained connection together. Page-cache targets are not hard
reservations: this calculation alone does not qualify their combined peak.

Runtime scopes retain one original monotonic clock/deadline and 8000000
interruptible VM steps. Opening validates only fixed schema/header state
under that same bound. Full integrity maintenance and explicit cold
IndexStore::maintenance_view capture have a separate 1099511627776-step
ceiling derived from the 8 GiB physical cap, under an explicitly supplied
deadline. A maintenance view installs its allowance only at capture,
before BEGIN and account endpoint lookup, and retains it for all
metadata/body work. It uses the ordinary reader pool and snapshot/drop
rules; a returned slot is reset to the ordinary allowance by view.
Neither a larger allowance nor a fresh operation extends the captured
deadline or clears failure within a live view. An interrupted maintenance
scan reports failure without making the store impossible to open. Clock
reversal or work failure stays sticky. Account creation, object/history
transactions, view capture, integrity validation, usage fences and
checkpoints carry the clock observation from writer-fence acquisition
into native scope initialization. A reversal across that handoff returns
Invalid before SQL work. A refused view capture returns its unused
reader slot; a later independently admitted request may start a fresh
scope. COMMIT and ROLLBACK finish without progress interruption; a late
clock sample does not obscure the actual durable result. Busy never
extends a deadline.

### Atomic body and metadata commit

IndexStore::commit accepts typed operations and mutable BlobSource inputs.
IndexStore::commit_batch instead borrows a complete format::batch::Batch.
Both use the same transaction implementation and original native scope.
Encoded operations reborrow original immutable bytes using checked offset
slots; no second row arena or operation array is built. Nested source and
submission matching decode only the relevant fixed-size keys, not row values.
The caller retains input/slot admission and prepared-source ownership.
Local framing validation grants no account or reservation authority.
Each source identifies a BlobId and supplies a std::io::Read. Each source matches exactly one Blob PUT. The matching
BlobRow supplies the exact expected length, SHA-256, kind and creation time.
The writer admits the maximum length before inserting a row, then inserts and hashes bounded chunk rows. Exact length, physical
source EOF and digest must agree before COMMIT. Readers may wrap caller-owned
provisional files, but those files carry no durable store authority.

One writer mutex serializes BEGIN IMMEDIATE, expected account-sequence
comparison, body streaming, relational changes, final reference/parent checks,
sequence update and COMMIT. A rejected source or batch rolls back both body
and metadata. No separate publication proof, body rename or permanent-body orphan
collection is required. The separate provisional ingress store below supplies
prepared file inputs; service admission remains separate. Existing body
identity and bytes are immutable; an identical Blob PUT is a
no-op that retains its original changed sequence. Every chunk has at most
65536 bytes and a consecutive ordinal; empty bodies have no chunks. No SQL
statement assembles a complete message or rewrites an existing body. Blob IDs have a
permanent native registry and cannot be reused after deletion.

Native deferred foreign keys enforce final owning relationships even when a
target is deleted. Bounded parent walking also refuses mailbox cycles. Exact
unsigned account sequences use eight-byte big-endian blobs, preserving values
above i64::MAX; overflow refuses mutation. Account creation is bounded to 128.
Account endpoint and history-floor reads require exactly eight bytes.
The decoded history floor must not exceed the committed endpoint; an
incoherent account identity returns Corrupt during view capture or before
transaction mutation. Explicit history retirement is described below;
ordinary object commits preserve the floor.
For each change row selected by the native range query, the sequence must
have eight bytes and the object ID sixteen. Short or oversized blobs
return Corrupt; the reader never pads them or treats their stored width
as caller scratch exhaustion. These checks do not scan unselected rows.
A selected change's stored operation ordinal must be 0 through 4095,
below MAX_OPERATIONS; an out-of-range value returns Corrupt. The caller's
u32::MAX cursor remains a valid instruction to skip a sequence, never a
valid stored ordinal.
Change actions must agree with pre/post existence, and a unique native index
refuses duplicate changes for one object within a transaction.

Before applying rows, a surviving Email PUT must retain the thread ID from
its original account transaction row. Repeated keys and DELETE/reinsert
cannot change that assignment within one transaction. A different final
thread returns Conflict before body streaming or row writes, even when
both threads exist. Intermediate replacements followed by the original
assignment or final deletion are allowed by this guard. New Email rows
have no original assignment to compare. Initial thread selection, anchor
maintenance, ID allocation/reuse and mutation authorization remain service
obligations; this guard preserves only the existing thread assignment.

Before applying rows, a final DELETE of an existing Submission requires an
original completedAt. An intermediate completion PUT or DELETE/reinsert
cannot supply that prior durable completion. This prevents deletion of an
existing unfinished consistent group; it does not authorize deletion of a
completed group. Retention age, category, administrator authorization,
acknowledgement and storing a Pending notice before deletion remain service
obligations. Operations on new rows have no
original history for this guard to compare. Recipient DELETEs remain under
final ordinal coverage and foreign-key checks.
For surviving PUTs, compare each existing Submission/Recipient with the
original account transaction snapshot.
Submission email/thread/identity, transmitted blob, reverse path, creation
and expiry times, and recipient count are immutable. An existing
submission cannot change a None notice directly to Stored: Pending must
commit first. An intermediate Pending PUT or DELETE cannot satisfy that
prior commitment. If the original submission was already completed with
None, its notice must remain None. A Pending failure notice cannot return
to None. A Stored notice must remain Stored with its original historical
notificationEmail, even after the visible Email is deleted.
Cancellation preserves either retained notice state. For an existing
None-to-Pending update, a surviving final Canceled recipient PUT for the same
submission causes Conflict. With the final whole-group checks, this prevents
first cancellation creating a notice from an initially consistent group:
an unfinished valid group contains no Canceled recipients, so its first
cancellation must write one. An already completed None submission cannot
gain Pending either. This does not replace startup validation of the
original group.
Recipient address is immutable and uncertainty cannot clear. A changed
attemptCount must be exactly one checked increment, with an attempt ID
different from the immediately previous row. Its original state must be
Queued, RetryWait or OutcomeUnknown with a next attempt, and its final
state must be InFlight/Prepared. An active InFlight attempt cannot be
replaced by incrementing the count. A count-advancing Prepared update must
carry an empty diagnostic; nonempty text refuses with Conflict rather than
being normalized by the adapter. Prepared and a later phase cannot be
combined into one count-advancing transaction: the final PUT is compared
with the original row. This constrains stored history; it does not prove
when the caller performs transport I/O.
If the original recipient is not InFlight, both reply fields stay
unchanged, including on dispatch into a new Prepared attempt. A final
Canceled row also retains both original replies, including cancellation
from InFlight/Prepared or InFlight/Body. Cancellation cannot invent,
replace or clear either reply. An intermediate Prepared PUT or DELETE
cannot authorize a reply change in that transaction. After Prepared
commits, the active attempt can record applicable replies. Once the
original InFlight row is in Body or AcceptancePossible, its RCPT reply
must remain unchanged in the final PUT, including outcome transitions. A
later attempt can record a new RCPT reply after its own Prepared commit.
Applicable DATA outcomes can still be recorded with the retained RCPT
reply. Matching updates to a current worker and enforcing remaining reply
ordering remain service obligations. For other local outcomes, including
expiry, restart recovery and a local abort, the core does not check either
reply from an original Prepared row, or DATA from later active phases. The
coordinator must refuse reply changes and invented DATA replies for those
local outcomes. Final group and acceptance-exposure checks still apply.
Both reply fields on terminal recipients remain fixed.
When attemptCount is unchanged, attempt ID and lastAttemptAt stay unchanged.
An InFlight final row then also requires an original InFlight row: re-entry
into InFlight cannot reuse a pending recipient's previous count/ID/time.
When both original and final rows are InFlight, the phase may stay the same
or advance one step: Prepared to Body, then Body to AcceptancePossible.
AcceptancePossible cannot return to an earlier phase. A final
AcceptancePossible PUT against an original Prepared row refuses even when
an intermediate PUT names Body: Body must commit in a separate transaction.
An existing row may become Accepted only from InFlight/AcceptancePossible;
an already Accepted row may stay Accepted under the terminal checks below.
An original InFlight/AcceptancePossible row cannot become Canceled, even
when its uncertainty bit is false. These boundaries compare the original
row with the final PUT: AcceptancePossible and Accepted cannot share one
commit from Body, and an intermediate phase or DELETE cannot make an
exposed attempt cancelable. Stored phase history does not prove current
reply provenance, worker fencing or actual transport ordering.
Other outcome transitions, such as RetryWait becoming Failed with reason
SmtpPermanent when a retained reply satisfies the final-state checks, are
still accepted by this core when the final group is otherwise valid.
They remain explicit coordinator-validation gaps, not proof of a fenced SMTP
result. From InFlight/AcceptancePossible, a certain RetryWait requires
SmtpTemporary and a stored DATA 4xx; Failed requires SmtpPermanent with a
stored DATA 5xx, or Expired with a stored DATA 4xx/5xx. Both require the
original row's DATA reply to be absent: QUEUE.md requires the new RCPT reply
to clear old DATA history and forbids storing interim 354. A retained negative
DATA reply cannot justify a certain failure/retry from an older exposed row;
this is a defensive history check, not support for that stored shape. Recovery
and final-state validation refuse InFlight/Body or AcceptancePossible with
retained DATA. No queue worker has shipped and no migration of such rows is
provided. The history check alone does not prevent an older exposed row with
retained positive DATA from becoming Accepted; current result authority still
requires the coordinator. OutcomeUnknown remains available for uncertainty. The shared
stored-reply classifier checks the code and separator; an RCPT refusal alone
is insufficient.
Other certain retry/failure shapes refuse, so a network/protocol failure
cannot discard exposure and permit later cancellation. Stored negative
replies are necessary here, but do not prove a definitive result from the
current attempt. The coordinator must establish that provenance and record
disconnects after exposure as OutcomeUnknown with uncertainty latched.
Accepted and Canceled recipients remain in their respective states.
Failed recipients remain Failed or become Canceled as part of a valid
whole-submission cancellation. OutcomeUnknown without a next attempt remains
OutcomeUnknown without a next attempt. All these terminal rows retain their
attempt count, so their attempt ID/time cannot change either. They also keep
phase and uncertainty unchanged; reason stays unchanged except
for Failed becoming Canceled. Diagnostics can still change. Reply comparison
is bounded by the existing row limits and borrows the decoded strings. This
preserves recorded history; it does not prove the original replies' authority.
A mismatch returns Conflict before body streaming or row writes.
A DELETE followed by a PUT cannot reset that history within the transaction;
intermediate PUT values have no separate durable effect.

The scan projects Email, Submission and Recipient keys without decoding
unrelated rows and reuses writer scratch. The outer pass makes one
history-key projection per position and at most 4096 further PUT-key
projections to skip Email and Recipient DELETEs. Those DELETE positions
and operations without an original row skip the later-key scan.
Submission DELETEs read original rows and check final effects alongside
PUTs. At most 4096 original point reads and 8386560 later-key
projections are possible; repeated keys may repeat an original point
read.
The quadratic scan samples the original deadline at least every 64 bounded
key projections and before final value decoding; all SQL shares the
original native allowance. The additional notice scan visits every
operation position, projecting queue PUT keys for each qualifying final
submission PUT and checks later keys before decoding matching survivors.
With N operations, S qualifying submission PUTs and M recipient PUT
positions belonging to those submissions, S + M <= N: each recipient
belongs to at most one qualifying submission. The additional projections
are at most S*N + M*N <= N*N, or 16777216 at N=4096, plus at most 4096
recipient value decodes. The combined later-key and additional-scan
ceiling is 25163776 projections. The notice scan adds no SQL reads or
allocations. Each forward scan checks the original deadline every 64
projections; the notice scan also checks immediately after its nested
later-key scan, including superseded PUTs, and before decoding. The
maximum-batch tests cover 4096 distinct Email PUTs, recipient history
and 2048 simultaneous notice completions. These are fixture evidence, not
a promise that every store/host can finish within its deadline.
This preserves named history fields, not the full transition graph. The
counter/ID comparison does not authorize dispatch or prove random/global ID
freshness: the service must obtain a fresh ID from Entropy, reject collisions,
authorize dispatch timing and the worker, and wait for the Prepared commit
before transport I/O. The stored source/destination shape alone does not
establish any of those permissions or transport ordering.
The core compares only the immediately prior ID; it retains no AttemptId
registry and does not establish worker fences. Counter exhaustion still
requires service pause and operator intervention. Actual notice creation
and creation authorization remain service obligations. Once completedAt is
present, surviving submission PUTs must preserve its exact value, including
during whole-group cancellation.
An intermediate unset PUT or DELETE cannot reset that original observation.
A pending row may retain absence or acquire its first completion timestamp;
final group checks still require completion exactly when obligations end.
The core does not validate the timestamp against the current clock or sendAt;
clock validation and retention/deletion authorization remain service work.
Preserving notice history does not prove the referenced failure Email was
created.
The service must authorize initial notice state on newly created rows and
atomically create the failure Email with the Stored transition. New rows
have no original history for this guard to compare, and Stored still does
not require Email creation; these are explicit service-layer gaps.
Once committed, the retained history cannot be repaired by clearing the
notice state or replacing its historical Email ID.

Before COMMIT, every submission named by a Submission or Recipient PUT/DELETE
is checked once against the final transaction view. Reuse the recipient
sweep's QUEUE.md state and aggregate validator: require exactly ordinals
0 through recipient_count-1, valid recipient states/replies, matching
completedAt/notification state and whole-submission cancellation. Missing or
extra recipients and inconsistent groups reject the entire transaction,
including streamed bodies and changes. Deleted parents rely on deferred
foreign keys to reject any surviving children. Repeated row operations retain
their final effect, regardless of caller order.

One group uses a parent point read, at most 1000 recipient point reads and
one indexed successor lookup to exclude trailing recipients. That lookup may
inspect the next group's first row; unrelated groups are never enumerated.
Deduplication examines
at most the bounded 4096-operation batch, using fixed scalar/key state and
the existing writer scratch. All groups share the original native deadline
and VM fuel; exhaustion rolls back rather than admitting a partial check.
Group checks establish final consistency; the separate pre-transaction
comparison preserves the named history fields above. Neither grants worker
fences, authorization, complete transition validation or SMTP acceptance.

Successful synchronous FULL COMMIT plus autocommit proves durability and
returns its sequence, including when the deadline expires during completion.
Pre-COMMIT failures and deferred-constraint/busy refusals reject after rollback;
failed rollback retires the writer. Other COMMIT failures are indeterminate
and stop writes until reopen/recovery. New read snapshots remain available.
Never acknowledge before a proven successful COMMIT. Queue transitions,
authorization, result idempotence and ports::Store
coordination remain service work; the low-level core grants no permission.

### Snapshots, changes, reclamation and backup

The cold pool owns one to eight connections. Capturing a view under the writer
fence begins a SQLite read transaction and reads its account endpoint to
establish the snapshot. Every lookup and body read uses that same snapshot
and original work scope. Capture may return Busy while a commit streams a
body; already captured views remain usable. ViewIdentity contains account,
epoch, committed sequence and history floor. SQL columns are decoded into
caller buffers, without loading a mailbox into RAM.

A verified body input and completed PinnedBlob borrow the live view.
Indexed chunk reads use the retained read transaction to preserve
identity. A read error that ends that transaction permanently fails the
view; later operations cannot silently switch snapshots. Length/digest
verification precedes completed random access. Before issuing the
completed pin, one closed query also rejects negative chunk ordinals and
ordinals beyond the declared body's final chunk. Two primary-key range
existence probes use the same account/blob and retained snapshot,
without reading extra body payloads. Empty bodies require no chunk rows.
The original native deadline and VM allowance cover this check; failure
grants no completed pin. Together with sequential chunk length/digest
verification, this checks the selected body's exact extent, not the rest
of the store. The borrow and explicit destructor keep the pooled
connection loan live until the body owner is destroyed, including when
held inside a MIME owner.
The native connection retains one shared sticky snapshot-loss error for
metadata reads, body-input construction/polls/completion and completed
pin reads/freshness checks. Each boundary checks SQL transaction liveness
before and after work, including failed work; loss hides any success and
preserves the operation error when one exists. A newly begun transaction
cannot revive that captured identity. These checks add no SQL or clock
samples and grant no replacement snapshot. The fixed error cell is part
of the cold connection owner, with no per-poll allocation.
Drop rolls back the read transaction before returning the connection;
snapshot loss or failed cleanup closes and retires the slot. Cleanup
bypasses expired request fuel without clearing its sticky failure.
Reopen restores retired capacity.

Deleting an unreferenced blob removes body and metadata transactionally. Old
views can still read its old bytes through their WAL snapshot. SQLite reuses
freed database pages; logical deletion does not shrink the main file. Leases
retain upload bytes until the lease row is explicitly deleted, even after
expiry. Expiry or revocation removes permission to use an upload, not its
foreign-key ownership. No custom per-file pin registry is needed.

A bounded native regression exercises public deletion with all eight
reader slots borrowed over a patterned 2 MiB body. In separate cases,
the writer commits after the first chunk of each input or after all
eight digests have produced pins. Every input completes its original
body and every pin reads all original bytes after deletion. Retained
views keep their original typed row and full identity; a newly borrowed
slot sees sequence two and no body while seven old views still see
sequence one. Ninth captures refuse Busy, checkpoints refuse while old
views remain, and all eight slots can be reacquired after release.
Checkpoint/reopen passes physical integrity and preserves deletion and
permanent-ID reuse refusal. This is sequential native snapshot evidence
for the bounded fixture, not parallel-thread, portable resource,
maximum-database, fault or service qualification.

IndexReadView::logical_usage returns passive LogicalUsage totals with the
captured ViewIdentity. Count every current BlobRow once, including zero-byte
and unreferenced bodies. Upload bytes count bodies with a retained lease;
queue bytes count bodies with at least one retained submission, regardless
of how many submissions share them. Expired leases and completed submissions
remain charged until explicit row deletion. Queue count includes every
retained submission. Current typed foreign keys restrict leases to Upload
blobs and submissions to Message blobs; LeaseUse::Both names permitted
upload uses, not overlapping categories. Body bytes are independent of
category references; deleting a category row alone never reduces that total.

Schema constraints keep each body length in 0..32 MiB; with the fixed
8 GiB logical database ceiling, valid integer sums fit within SQLite i64.
Negative aggregate values are rejected before conversion to u64.

Two closed aggregate queries use the same account snapshot and original
native deadline/VM budget. Correlated existence checks use the lease primary
key and submissions_blob index; no row collection or deduplication array is
materialized in Rust. Failure yields no partial report and cannot renew the
scope. This is synchronous native work and may exhaust the bounded view on a
large store; it promises neither a coordinator scheduling turn nor completion
at the physical store ceiling. Old views keep old totals after commits.
The report is not a quota reservation or a durable-effect ticket. Summing
arbitrarily timed account views does not establish current global usage.
Database/WAL files, free pages, pending reservations and disposable
spools are excluded from this logical report.

IndexStore::usage_fence supplies a cold whole-store logical capture. It takes
the healthy writer fence, reads at most 128 accounts in one SQLite read
transaction, reuses the same indexed account aggregates and checks scalar
addition. One original deadline and the normal native VM allowance govern
the whole scan.
After successful read-transaction rollback, UsageFence retains the writer
mutex until drop. Its StoreLogicalUsage remains current while that fence is
held; existing views can still read their old snapshots. New views, account
creation, mutations and maintenance return Busy. Failure exposes no partial
capture; failed rollback stops the writer until recovery. Indeterminate
writers cannot issue a fence.

The existing admission::logical::Leases constructor consumes global
quota::Usage counters, with no per-account quota dimension. The fence
therefore retains only whole-store totals. It is for cold coordinator
initialization, not a runtime admission loop. It has no automatic timeout/drop
or worker-quiescence authority. The future coordinator must stop outstanding
workers and reconcile pending effects plus disposable usage before ledger
initialization. Copied totals remain passive observations and may become
stale after drop.
The normal native work ceiling may refuse large stores; whole-store maximum
resource qualification and service activation remain open.

After ending the read transaction, the fence also captures StoreFileUsage
under the same mutex and original clock. The mandatory database and optional
WAL must remain regular owner-only single-link files under their existing
length ceilings. An absent WAL contributes zero. File lengths include
reusable main pages and retained WAL tails; they are logical file extents,
not a free-space probe or a measurement of filesystem allocated blocks.
Metadata failure or expiry returns no fence and never substitutes zero for
an unmeasured present file. The read transaction is already closed on these
failures. Capture relies on the existing trusted stable-path contract; it
does not bind SQLite's open descriptors to these paths or detect namespace
replacement. An absent WAL is valid only within that contract.

WAL-only committed pages do not yet enlarge the main-file extent. Future
runtime coordination must account for checkpoint growth as well as write
and rollback growth; initializing these counters reserves no future growth.
The fixed native 8 GiB main-file ceiling also equals every valid plan's
DatabaseBytes cap. SQLite's separate 34 MiB WAL-index mapping allowance
(RESOURCES.md) is outside these two captured file buckets.

UsageFence::initialize_leases consumes the capture, combines its five
logical counters and two file extents with trusted AuxiliaryUsage for sort,
response, cache, log and cold state, and invokes the existing logical Leases
constructor on empty caller-owned cells/slots. All twelve global quota
buckets must fit their plan; pending charges start empty. The original
clock is checked before and after construction. Success returns a
ledger and releases the writer fence; failure releases the fence without
occupying cells or changing the database. The caller must quiesce other
resource owners, settle outstanding effects and control publication of the
initialized ledger. This helper does not enforce global ledger uniqueness or
bind the returned ledger to a store; trusted coordination must do both.
No duplicate used-counter table, reservation, effect
ticket or authenticated mutation authority is created.

Changes use the native account/kind/sequence/operation indexes.
IndexStore::prune_history supplies explicit bounded history retirement and
physical row reclamation. Its HistoryPruneRequest identifies an account,
expected committed endpoint, inclusive through sequence, max_rows in
1 through 4096, and one deadline. The trusted caller owns retention policy,
account authorization and resource admission; there is no automatic pruning
or retention scheduler. A changed endpoint or a through sequence below the
current floor returns Conflict; a future boundary or invalid row limit returns
Invalid. The account must exist and its captured counters must be coherent.

Under the existing writer mutex, BEGIN IMMEDIATE and original native scope,
pruning raises the floor to through without advancing the committed endpoint.
It selects at most max_rows plus one retired keys through the changes primary
key (account, sequence, operation), retaining only a count and last key. The
selected keys, including lookahead, require exact eight-byte sequences and
operation ordinals from 0 through 4095. It deletes at most max_rows entries
in primary-key order, verifies the deletion count, and commits the floor and
deletion together through the ordinary commit-outcome machinery. No bodies,
object rows or new change events are written. No schema migration is needed.
This is a bounded selected-prefix check, not a global history audit.

HistoryPruned returns the committed ViewIdentity, removed row count and more
flag only after known successful COMMIT. The flag describes retired rows
remaining in that transaction snapshot. Repeating through equal to the floor
continues cleanup; a zero-row result is valid. Partial physical deletion of a
sequence is safe because the entire sequence is already retired atomically.
New views reject cursors below the floor with HistoryLost. At a nonzero
floor, only operation u32::MAX, the completed-sequence sentinel, is accepted;
other operation cursors return HistoryLost because unconsumed changes may
have been retired. This conservative rule also refuses operation 4095.
The completed-sequence sentinel skips the entire floor sequence, including
rows awaiting reclamation. Floor-zero cursors retain their original rules;
ordinary writes never produce sequence-zero changes.
Retained WAL views keep their old floor and complete original history.
The receipt is passive metadata, not ownership of a view or admission credit.

Pruning shares the ordinary 8000000-step native budget, deadline, WAL room
check, disk/heap bounds, rollback handling and Rejected/Indeterminate commit
classification. An indeterminate outcome stops the writer until recovery.
A row cap bounds deletion work, not elapsed native I/O or completion at every
physical store size. Callers may retry smaller row caps after Capacity.
Reclaimed rows do not imply shorter database/WAL files or released ledger
charges: free pages, checkpoints, retained views and quota reconciliation
remain distinct. Runtime retention scheduling and admission integration are
unimplemented; the hard database ceiling may still refuse writes.

A native full-reader-pool fixture now qualifies bounded public history
pruning with eight retained partial body inputs. Public commits create a
patterned 2 MiB body, parent mailbox and Created change at sequence one,
then update the parent with an Updated change at sequence two. All eight
views capture the full sequence-two/floor-zero identity and each input
reads its first exact 64 KiB chunk before pruning.

With all eight loans alive, prune_history at expected sequence two,
through sequence two and max_rows one returns sequence two/floor two,
one removed row and more true. A second call removes one row with more
false; repeating returns zero and false. The endpoint does not advance.
All eight inputs then finish the remaining 31 chunks, verify every
original byte and complete independent digests; all pins pass
cross-chunk and final-byte reads. After pins drop, all eight old views
preserve their original full identity, typed BlobRow at changed sequence
one, updated parent at changed sequence two, both exact original change
records and completion.

A ninth capture and checkpoint refuse Busy while the pool is full.
Releasing one slot admits a current sequence-two/floor-two view
alongside seven old views: below-floor history returns HistoryLost, the
completed-floor sentinel returns Complete, and the old views retain both
changes. Releasing all views permits simultaneous capture of all eight
current slots with exact identity, typed rows and retired-history
behavior. Checkpoint then succeeds. Reopen with eight readers passes
physical integrity and repeats current identity, typed metadata, history
and complete body-byte/digest verification in every slot.

This is bounded native semantic evidence for sequential pruning with
eight same-account/same-body loans. The fixture uses public mutations
and pruning, without private SQL seeding, database-file opens, new fault
hooks, production changes or widened unsafe allowances. It does not
qualify portable allocation, RSS or guarded-stack limits, parallel
threads, multiple accounts, maximum-database work, retention-policy
authority, quota reconciliation, full filesystem faults, power loss or
whole-service scheduling.

A second native full-reader fixture qualifies public backup with retired
history still awaiting physical cleanup. It retains the same eight
partial 2 MiB body inputs, raises the history floor to sequence two with
max_rows one and deliberately leaves the receipt more true. Old views
retain both original changes while fresh views already reject
below-floor cursors. Complete body/digest checks, slot reuse, checkpoint
and reopened eight-reader checks run before consuming backup.

The real public backup returns the original epoch and a positive
page-aligned main-file extent within the public database ceiling,
matching the destination metadata. Both cooperative locks remain Busy.
Source and copy reopen with eight readers each, retaining eighteen
native owners and sixteen simultaneous current views. Both pass physical
integrity; every view checks complete sequence-two/floor-two identity,
exact typed body and updated parent with original changed sequences,
below-floor HistoryLost, completed-floor Complete and all original body
bytes and digest completion.

While those sixteen views remain retained, cleanup in the copied store
returns one removed row with more false, then zero and false.
Independent source cleanup subsequently also returns one and false, then
zero and false. This verifies that backup preserved the pending retired
row and that cleaning the copy did not clean the source. Full
identity/typed metadata/history/body verification repeats through all
sixteen retained views after cleanup. Ninth captures and checkpoints
refuse Busy in each full pool. After views drop, both checkpoints and
physical integrity checks pass.

The earlier fully cleaned full-pool fixture remains as a separate test
through the shared scenario. This is bounded native sequential
backup/pruning evidence for one account and body, without private SQL
seeding or independent database-file opens in the fixture. The backup
receipt alone is not semantic verification or restore approval. No
production, unsafe, hook, dependency or cap change is introduced;
portable resource limits, parallel work, multi-account/maximal stores,
normal service retention/admission, full filesystem faults and power
loss are not qualified.

A third native full-reader scenario renews the copied epoch while one
retired history row still awaits bounded cleanup. The original fully
cleaned and pending-backup tests remain independently named through the
shared scenario. After public backup, the copied store consumes one
deterministic test-entropy fill of exactly sixteen bytes, renews to the
independently expected fresh epoch, closes and reopens before any copied
view is captured. The source retains its original epoch.

Both stores reopen with eight readers each and retain sixteen
simultaneous views and eighteen native owners. Every view checks the
independently expected source or copied epoch, sequence two, floor two,
exact typed body and updated parent with original changed sequences,
below-floor HistoryLost, completed-floor Complete, every original body
byte and full digest. While all views remain retained, copied cleanup
returns one removed row with more false, then zero and false;
independent source cleanup returns the same receipts under its original
epoch. All sixteen views repeat the complete checks after cleanup; ninth
captures and checkpoints remain Busy before and after cleanup. After
views drop, both checkpoints and physical integrity checks pass.

This is bounded native sequential epoch-renewal and pending-cleanup
evidence for one account and one 2 MiB body. Deterministic test entropy
establishes semantic identity expectations, not SystemEntropy
qualification. No production, unsafe, hook, dependency or cap change is
introduced. Portable allocation/RSS/stack limits, parallel work,
multi-account/maximal stores, runtime retention/admission, complete
filesystem faults, power loss and whole-service behavior are not
qualified by this fixture.

A separate bounded native fixture qualifies two accounts with identical
BlobId and MailboxId values but different patterned 2 MiB bodies,
digests and mailbox names. Public mutations leave the first account at
sequence two with floor two and one retired history row pending cleanup;
the second remains at sequence one with floor zero and an exact Created
history record. A real public backup preserves both accounts. The copy
renews through one deterministic sixteen-byte test-entropy fill and
closes/reopens before view capture; source and copied epochs have
independent fixed expectations.

Both eight-reader pools alternate four views from each account and
retain sixteen views and eighteen native owners together. Every view
checks complete explicit account/epoch/endpoint/floor identity, exact
typed rows and changed sequences, account-specific history, every
original body byte and full digest. Copied cleanup of the first account
returns one removed row with more false, then zero and false; source
cleanup independently returns the same receipts under its original
epoch. All sixteen retained views repeat the complete checks after
cleanup. Ninth captures and checkpoints refuse Busy before and after
cleanup. After views drop, both stores checkpoint, pass physical
integrity, close and reopen. Fresh views repeat all checks for both
accounts, including the original history record of the second account;
this checks persisted account isolation beyond the old retained
snapshots.

This full-reader fixture family is serialized in the native test process
because SQLite shares its unchanged 16 MiB heap cap process-wide; each
fixture retains its full simultaneous pool and view assertions. This
qualifies only the bounded sequential two-account dataset. Deterministic
test entropy is not SystemEntropy qualification. No production, unsafe,
hook, dependency or cap change is introduced. Portable resources,
arbitrary-account/maximal stores, parallel service work, runtime
retention/admission, complete filesystem faults, power loss and
whole-service behavior remain separate.

Backup must capture one consistent SQLite state. Stop service activity,
checkpoint successfully, close every connection, then copy the main database;
that snapshot contains bodies and metadata together. Copying only the live
main file can lose committed WAL contents. The offline IndexStore::backup
primitive implements this sequence with a consuming engine owner and
caller-owned 64 KiB scratch. It retains the source
LOCK and an exclusively borrowed destination LockedRoot until completion.
A live borrowed view or body prevents consumption; a forgotten-view marker,
stopped writer or poisoned mutex refuses. Every remaining available connection closes
explicitly and must report success before a normal File opens the source.
Retired slots retain no native owner and rely on their earlier retirement
cleanup and the successful checkpoint. An implicit Drop of an available
connection is not accepted as evidence of successful close.

The destination must have no database, WAL, SHM, backup-partial or legacy
FORMAT entry. Its filesystem must support hard links and directory sync.
Create metadata.sqlite3.backup-partial exclusively as mode 0600, restoring
owner permissions after umask filtering. Copy exactly the checkpointed length,
at most 8 GiB, require source EOF, and sync the completed file. One original
clock/deadline spans checkpoint and copy; native scope observations hand off
to the copy scope. Each copy iteration checks before reading and writing.
Synchronous filesystem and native operations cannot promise interruption at
the deadline. This primitive reserves no quota or filesystem space.

Publish with an atomic no-replace hard link to metadata.sqlite3, unlink the
partial name, then sync the destination directory. Success returns the store
epoch and copied byte count only after that final sync. Once the final link
exists, cleanup and directory sync finish without another deadline refusal
that could obscure completed publication. An error before attempting the
link is Unpublished. Any error once linking is attempted is
IncompletePublication: the final name may exist, but cleanup and publication
durability are unproven. Even a failed link can have uncertain filesystem
effects. Neither error grants a completed backup; either can leave partial
artifacts. A crash between link and unlink
can leave two links to the same completed inode; ordinary open safely refuses
its link count. Under both locks, offline inspection must establish identity
before removing the partial link. Never auto-delete or overwrite a destination
on an error. The engine is consumed even on refusal; callers may reopen its
source while retaining the original lock.

The ordinary public copy-refusal oracle passed four cases over a healthy
2 MiB body and mailbox. An armed supplied clock observes a real 64 KiB
partial before expiring the original deadline, reversing monotonic time,
or returning a sample error. Each returns Unpublished with its exact
reason. A fourth case creates an occupied final name only after the
partial reaches the checkpointed source length; the real no-replace
link returns AlreadyExists as IncompletePublication. The complete
partial and occupied final name remain distinct and unchanged.
All cases retain both caller-held locks and a mode-0600, one-link partial
whose every byte matches the source prefix. Destination open refuses,
source reopen passes physical and complete account verification with
the original identity, typed rows and body contents, and backup retry
returns Conflict without changing retained artifacts. Source byte-file
reads occur only after consuming backup has closed every native owner.
The supplied sample error is not a filesystem write or sync fault. This
bounded oracle does not qualify full filesystem faults, successful
link/unlink crash windows, power loss or resource/stack ceilings.

The ordinary backup abrupt-death oracle kills only its recorded child
with SIGKILL in two phases. During copying, a test clock observes an
actual nonempty, incomplete partial file before publication and parks
the child. After success, the child parks only after backup returns its
receipt. The parent reacquires both root locks, checks the source's
physical integrity, captured account metadata and every body byte,
and distinguishes the incomplete partial from the completed final file.
An incomplete destination cannot open or be overwritten by a retry;
its partial file remains for inspection. A returned backup reopens with
the same epoch, sequence, metadata and body contents. This is a bounded
2 MiB process-death oracle, not power loss, the link/unlink window, sync
errors, complete filesystem faults or maximum-size backup qualification.

The separate --sqlite-backup resource mode passed a public offline copy
of one maximum-size 32 MiB body with parent/child mailboxes. Ten ordered
observations include complete source-account verification, returned
backup, and independently reopened source and destination with physical
and complete account checks. Both two-reader stores remain live at the
last verification point. RESOURCES.md states the requested-byte, warmed
teardown and sampled RSS limits; td-crypto/PORTABLE.md records the
isolated artifact evidence. This bounded fixture does not qualify an
8 GiB database, arbitrary-account maintenance, full filesystem faults,
power loss, guarded stack or whole-service overlap.

The separate ignored maximum_database_backup_preserves_complete_account
fixture qualifies a public backup at the exact 8589934592-byte database
ceiling. It reuses the maximum-database public admission helper below,
checks all 2097152 pages, and passes one caller-owned 64 KiB buffer to
the consuming backup. Require the original epoch and exact byte count in
the receipt, both main-file lengths equal to 8 GiB and no destination
partial name after success. Keep both root locks while reopening source
and destination sequentially, with eight readers plus the writer in each
verification pass; this does not claim simultaneous eighteen-connection
overlap.

Each reopened root passes full physical integrity and complete account
verification, including expected typed Blob rows and changed sequences,
epoch, endpoint/history floor, permanent-ID count, declared counts/bytes
and original body digests. The receipt alone remains separate from these
verification results. The fixture does not call the test-only dirty-body
producer; backup performs its own public checkpoint and closes all
native connections before copying. The fixed-clock, between-call
30-minute bound and outer timeout have the same limitations as the
maximum-database fixture below.

After the optimized native build below, invoke its library test
executable with a private disk-backed TMPDIR containing at least 40 GiB
free:

```text
TMPDIR=/path/on/disk timeout --kill-after=5s 2700 target/release/td-builder run-capped "$td_mta_lib_test" --ignored --exact store_fs::index::database_qualification::maximum_database_backup_preserves_complete_account --nocapture --test-threads=1
```

Require exit status zero and exactly one passed test. Ordinary gates
leave it ignored. Both fixture roots are printed; after external
termination, confirm that the invocation and all descendants have exited
before removing only those owned roots.

The 2026-10-09 optimized x86-64 GNU host run used rustc 1.99.0-nightly
(6f72b5dd5), Linux 7.0.14 and btrfs. Public admission accepted 262
bodies with 16 clean Capacity refusals, reaching exactly 8 GiB without
padding. The public consuming backup returned 8589934592 bytes in 8.978
seconds. Source/destination physical scans took 15.891/12.473 seconds;
complete account passes each verified 8513712128 declared body bytes in
57.297/57.984 seconds. Exactly one test passed in 313.12 seconds with
zero failures. The native library test executable SHA-256 was:

```text
d4511df913b16db24e8f2181accf9f5c766f1a6d870aa29c01bbc69b3a6e3910
```

The unchanged 9 MiB individual and 16 MiB shared SQLite requested-
allocation caps remained active. Five RSS observations were 5640, 11016,
10984, 11356 and 11512 KiB; the largest reported VmHWM sample was 11512
KiB. This qualifies the specific body-dominated maximum-size copy and
separate reopen/verification paths. It does not qualify arbitrary-
account metadata, simultaneous pool overlap, wrapped allocation
attribution, transient RSS, guarded stack, filesystem faults, power
loss, backup credentials/configuration or operational
restore/repair/activation.

The snapshot contains authoritative bodies and metadata, but receipt success
does not verify every body digest or domain invariant. The snapshot may be
opened for offline inspection with ordinary IndexStore::open and its normal
checks. The digest-damaged backup oracle starts with a verified 2 MiB body
and mailbox, then changes the first 64 KiB chunk without changing its size.
Physical integrity still passes, but account verification refuses body
corruption. Public backup succeeds; both reopened roots retain the original
metadata, sequence/floor and declared digest, every damaged first-chunk byte
and every unchanged remaining byte. Full streamed digest verification
refuses, granting no pin or completed account report. This bounded case
qualifies preservation of same-size body damage, not arbitrary corruption,
structural damage, repair, power loss, full filesystem faults or
maximum-size backup resources.

Serving a restored snapshot requires a fresh epoch so old client state
tokens cannot identify a different history. IndexStore::renew_epoch
consumes the engine and obtains one 16-byte candidate from the caller's
admitted, warmed entropy source. A candidate equal to the current epoch
refuses with Conflict; there is no retry loop. All bytes from a failed
fill are discarded. Other 16-byte values follow the existing StoreEpoch
domain. Freshness across previous histories and other stores relies on
the admitted entropy source, not this single comparison.

Under the exclusive writer fence, a forgotten-view marker refuses with
Busy, the existing WAL headroom bound applies, and one original native
deadline/clock scope covers acquisition, entropy and SQL. Checks before
and after entropy fill preserve that scope even when fill returns an
error. Synchronous native entropy may block or abort; those checks do
not make it interruptible. BEGIN IMMEDIATE and a conditional store-epoch
update require the persisted old epoch to match the owner; a missing or
mismatched row refuses with Corrupt. The ordinary commit classifier
governs acknowledgement: only known durable success updates the
in-memory epoch and returns the owner, including success observed after
the deadline. Accounts, endpoints, floors, objects, body chunks,
retained changes and the used-blob-ID registry are preserved.

The restored-epoch process-death oracle first creates a real backup
with two accounts, retained history and a permanently used deleted blob
ID. One child parks inside the supplied entropy fill after preparing
its candidate but before fill returns or epoch SQL begins. A second
parks in the existing safe SQLite commit hook after the epoch UPDATE
and before that hook returns; a third parks after the returned owner
and new identity are checked, with an actual nonempty WAL retained. The
parent kills and reaps only that child and reopens the source and
restored snapshot. The source keeps its old
epoch; the snapshot retains the old epoch at the before-SQL and
before-commit cuts and the new one after the known return. Account
endpoints/floors, retained change, body bytes/digest, empty second account
and used-ID refusal remain intact.
Old client state matches both precommit identities. These three bounded
cuts do not qualify the entire UPDATE/COMMIT window, unknown commit
outcomes, power loss, entropy uniqueness, full filesystem faults or
service activation.

The separate --sqlite-epoch resource mode exercises the consuming renewal
primitive with an actual warmed td_crypto::SystemEntropy on its observing
thread. Its separate Rust, wrapped C-boundary and unwrapped RSS processes
commit one streamed 32 MiB body and parent/child mailboxes, perform real public
backup with reused 64 KiB scratch, and independently verify both roots.
A temporary pre-backup view supplies only a passive original identity.
Exactly one 16-byte public renewal fill must return a different epoch; the
destination identity changes only that field, old state becomes stale, and
new state retains itself. Full physical integrity and complete account checks
run after renewal, after destination checkpoint/reopen, and again on the
source, which retains its original identity and old state.

Both root locks remain held. Source and destination each have two readers
plus a writer; both stores remain live at the renewed, reopened and final
source observations. The thirteen-phase sequence extends the backup path
with epoch_renewed, epoch_reopened and source_preserved. The prior body,
account and backup modes keep their original phase counts and
controls. RESOURCES.md specifies unchanged requested-byte/RSS limits and
warm teardown requirements; td-crypto/PORTABLE.md records the successful
isolated static-musl observations and exact staged inputs. Warming entropy
before baseline does not claim that handle drop frees provider thread state.
This bounded resource fixture does not qualify maximum database/account,
entropy quality or worst-case latency, unknown outcomes, full faults,
power loss, guarded stack, whole-service overlap or restore activation.

The separate ignored
maximum_database_backup_epoch_preserves_complete_account fixture qualifies
known-success epoch replacement on a real body-dominated 8 GiB backup.
It reuses the bounded public admission and optional padding helper, captures
the original passive view identity and Email DataState, then consumes the
source into public backup with one 64 KiB caller buffer. The receipt must
match the old epoch and exactly 8589934592 bytes; the destination partial
name must be absent. Both root locks remain held throughout.

The destination opens with eight readers plus the writer, initially matching
the original identity and all 2097152 pages. Public consuming renew_epoch
uses exactly one deterministic test entropy fill of 16 bytes with a candidate
different from the old epoch. The returned identity must change only its
epoch; the old state becomes stale and the new state retains itself. Full
physical integrity, every expected typed Blob row and changed sequence,
endpoint/floor, permanent-ID count, metadata counts, declared bytes and
original body digests are checked under one maintenance view per account
pass, without per-body allowance renewal. Checkpoint and reopen retain the
new epoch and repeat those complete checks. Finally, the source independently
reopens with its original identity and retained old state and repeats all
content checks. Stores open sequentially, with nine native connections at a
time; the destination checkpointed main file and original source remain
exactly 8 GiB. No eighteen-connection overlap is exercised.

After the optimized native build below, run its library test executable with
a private disk-backed TMPDIR containing at least 40 GiB free:

```text
TMPDIR=/path/on/disk timeout --kill-after=5s 2700 target/release/td-builder run-capped "$td_mta_lib_test" --ignored --exact store_fs::index::database_qualification::maximum_database_backup_epoch_preserves_complete_account --nocapture --test-threads=1
```

Require exit status zero and exactly one passed test. Ordinary gates leave
this case ignored. Its fixed clock, between-call 30-minute bound, outer
timeout and owned-root cleanup have the same limitations and procedure as
the separate maximum-database fixture below.

The 2026-10-09 optimized x86-64 GNU host run used rustc 1.99.0-nightly
(6f72b5dd5), Linux 7.0.14 and btrfs. Public admission accepted 262 bodies
with 16 clean Capacity refusals, reaching exactly 2097152 pages without
padding. Backup copied 8589934592 bytes in 8.518 seconds; known-success
epoch renewal with one fill took 18.739 milliseconds, including the checked
post-renewal identity. Physical scans after renewal, after destination reopen
and on the source took 9.107, 8.726 and 32.466 seconds. Each complete account
pass verified 8513712128 declared body bytes in 58.013, 57.508 and 57.895
seconds respectively. Exactly one test passed in 390.55 seconds with zero
failures. The native library test executable SHA-256 was:

```text
5515590d41373e92e985ad3e52593f2a66951af0b49cca3297edd2e4bbb97d76
```

The unchanged 9 MiB individual and 16 MiB shared SQLite requested-allocation
caps remained active. Six RSS observations were 5592, 11204, 11172, 11412,
11576 and 11584 KiB; the largest reported VmHWM sample was 11584 KiB.
This qualifies this healthy full-database epoch primitive and preserved
body-dominated account, not entropy uniqueness, unknown commit outcomes,
arbitrary-account metadata, portable/wrapped allocation attribution,
transient RSS, guarded stack, full filesystem faults, power loss,
credentials/configuration snapshots, operational restore authorization or
service activation. No production storage, schema, unsafe, dependency,
allocator hook or native-cap change is introduced.

The shared portable body/account/backup/epoch fixtures also passed a
public usage fence with all eight partial body inputs retained. After
the metadata PUT commits sequence two, the fence captures exact passive
totals: the current epoch, one account, 32 MiB body bytes, one blob and
zero upload bytes, queue bytes and queue submissions. Main-file and WAL
extents are positive and within their respective public
SQLITE_DATABASE_BYTES and SQLITE_WAL_BYTES limits; the WAL limit
includes frame overhead. With the fence alive, another metadata PUT at
expected sequence two, checkpoint, a second fence and a ninth view all
refuse Busy.

The fence remains alive through the remaining 511 chunks of each input,
all eight complete original-body digests, pinned random reads and the
verified observation. Dropping the fence precedes dropping pins and old
views. All eight old views retain sequence one and original typed
metadata; all eight reacquired views expose sequence two and the updated
parent. Thus the refused fenced commit does not advance the endpoint.
All original selectors, phase counts, owner counts, copy/reopen/epoch
checks and rollback/refusal/retry controls remain.

The complete isolated pinned Rust 1.96.0 release x86-64 musl command
passed on its first attempt on 2026-10-09: all eight static artifacts,
API confinement, clean runtime, all resource and positive controls and
four guarded worker-stack cases. No production API, unsafe boundary,
syscall, dependency, reservation, cap or compiler flag changed.

This qualifies passive usage capture and bounded refusals with eight
same-account/same-body loans in one process or guarded worker.
initialize_leases is not called: these totals grant no quota
reservation, effect authorization or service quiescence. Whole-fixture
peaks and exact warm teardown do not isolate usage_fence allocations or
prove per-call allocation freedom. The complete suite also passes its
separate unchanged quiet allocation controls. Rust 2 MiB, wrapped
C-boundary 17 MiB, sampled RSS growth 24 MiB and guarded writable
mapping 256 KiB limits remain unchanged, as do SQLite 9 MiB
per-allocation and 16 MiB process-wide caps across all pools. No
parallel-thread, multi-account, maximum-database, transient-RSS, frame
high-water, full-fault, power-loss or whole-service claim follows.

RESOURCES.md and td-crypto/PORTABLE.md record actual observer
measurements and artifact inputs. Earlier metadata-writer evidence omits
this fence.

A separate portable sqlite-multi-account selector now qualifies two
public accounts sharing the same BlobId and parent/child MailboxId
values. Each has a distinct uniform 32 MiB body, digest and mailbox
names: 64 MiB total body bytes. Eight source views alternate four
captures per account, retain eight partial body inputs across a
first-account metadata PUT, and finish every byte, complete digest and
pinned read under a passive usage fence. The fence reports two accounts,
two blobs and exact 64 MiB body totals, zero upload/queue bytes and
submissions, and positive bounded main/WAL extents. Both accounts start
at sequence one and floor zero; the first advances to sequence two while
the second remains at one. Another commit, checkpoint, second fence and
ninth view refuse Busy while the fence is held. Old views preserve their
original metadata; reacquired views show the account-specific endpoints
and names.

The fixture consumes the source through actual public backup with a
positive capped receipt covering at least 64 MiB. Source and copy retain
both complete eight-reader pools and writers together: eighteen native
owners, without retaining sixteen source/copy views together. Physical
verification and complete account verification cover both accounts in
each store. Independent expected mailbox rows and changed sequences,
every expected account-specific body byte, and exact BlobRow length,
digest and changed sequence prevent a self-consistent cross-account
body/metadata swap from passing. Copied public renewal uses one
sixteen-byte fill from its actual warmed SystemEntropy handle; the
returned epoch must equal the independently captured supplied bytes and
differ from the original. Both copied state domains adopt that epoch, it
survives checkpoint/close/reopen, and both original source state domains
and contents remain unchanged.

The complete isolated pinned Rust 1.96.0 release x86-64 musl command
passed on its first attempt on 2026-10-09: all eight static artifacts,
API confinement (schema 57, 29 fixtures, 786 reachable items), clean
runtime, all existing resource/positive/quiet controls, five SQLite
observer cases and five guarded worker-stack cases. The four earlier
body/account/backup/epoch selectors remain independently runnable with
their original phase counts; the new selector has thirteen phases.
Explicit body and typed-row expectations also strengthen the existing
account verification paths, and epoch renewal now compares the actual
supplied entropy output. These measurements are a fresh run of all five
selectors, not a reuse of earlier artifact evidence.

This qualifies these two accounts and shared IDs in one process or
guarded worker, without history pruning. It does not qualify arbitrary
account counts, maximum-database resource use, parallel services, quota
reservation, effect authorization, initialize_leases, service
quiescence, full filesystem faults, power loss or whole-service
readiness. Lifetime requested peaks and exact warm teardown do not
isolate per-call allocations or assert an allocation-free SQLite
interval. Wrapped C observations can include Rust System allocations and
are not disjoint SQLite-only attribution. RSS is sampled at named
phases, including one first-body writing sample; it does not bound
transient RSS or separately sample the second body during writing.
Writable mapping size is not a frame high-water measurement. Rust 2 MiB,
wrapped C-boundary 17 MiB, sampled RSS growth 24 MiB and guarded
writable mapping 256 KiB limits remain unchanged, as do SQLite 9 MiB
per-allocation and 16 MiB process-wide caps across all pools. No
production API, schema, unsafe surface, syscall, dependency, compiler
flag, probe shim or stack wrapper changed.

RESOURCES.md and td-crypto/PORTABLE.md record actual observer
measurements and artifact inputs. The separate native two-account
pruning fixture qualifies its floor-two cleanup scenario; this portable
case retains floor zero.

The shared portable SQLite body/account/backup/epoch fixtures passed a
public metadata commit with eight retained partial body inputs. After
the initial 32 MiB body commit at sequence one, all eight views capture
the same full identity and each input reads its first exact 64 KiB
chunk. With every input and view retained, the writer puts the parent
mailbox with name updated parent and commits sequence two. Body mode
creates that parent; account/backup/epoch modes rename their existing
parent, preserving the three-row/two-mailbox dataset. Ninth captures
refuse Busy before and after this commit. The committed observation now
occurs after the metadata commit with all eight partial inputs alive.

The remaining 511 chunks per input progress sequentially in round-robin
order through the same caller scratch. Every original body byte and each
completed digest is checked; all eight pins pass cross-chunk and
final-byte reads and remain alive with their views at the verified
observation. After dropping pins, all eight old views retain their
complete sequence-one identities and the original parent row at changed
sequence one, or no parent in body mode. Releasing them permits
simultaneous reacquisition of all eight slots with full sequence-two
identities and the exact updated parent row at changed sequence two.
Those views drop before account verification or consuming backup.

The complete pinned static-musl command passed all four Rust/native/RSS
observer cases and all four guarded worker-stack cases with unchanged
counts and ceilings. RESOURCES.md and td-crypto/PORTABLE.md record
actual measurements and artifact inputs.

Selectors and the eight/nine/ten/thirteen observation counts remain
unchanged. Account verification, rollback/refusal/oversized and
unconsumed-source controls, permanent-ID retry, public backup, reopened
physical checks, source preservation and actual warmed entropy renewal
remain. Later account/source/destination identity checks use sequence
two; the successful body/account retry advances to sequence three. Each
store still owns eight readers plus one writer; reopened backup/epoch
source and destination stores retain eighteen native owners together.
Rust 2 MiB, wrapped C-boundary 17 MiB, sampled RSS growth 24 MiB and
guarded writable stack 256 KiB ceilings are unchanged. SQLite retains
separate 9 MiB per-allocation and 16 MiB process-wide caps across all
pools.

This qualifies the bounded public metadata PUT with retained
same-account/same-body inputs and sequential interleaved reads on one
process or guarded worker. It does not qualify parallel threads, every
write or deletion under portable resource observation, different
accounts/bodies, arbitrary-account or maximum-database work, numeric
frame peaks, transient RSS, complete faults, power loss or
whole-worker/service overlap. Requested-byte peaks and teardown describe
the combined fixture; they do not isolate allocations inside the
metadata commit. Earlier simultaneous-reader records remain tied to
their own artifact/source inputs and omit this metadata commit.

The earlier simultaneous-body-reader qualification retained eight
borrowed views and body inputs together during their initial postcommit
body verification. Fixed stack arrays retain all loans without new Rust
heap buffers. Every view matches the complete
account/epoch/sequence/floor identity; a ninth public capture returns
Busy. Progress interleaves one 64 KiB read from each input using the
existing single scratch buffer. Each input consumes all 512 chunks of
the same 32 MiB body, checks every byte and finishes its own digest. All
eight resulting pins pass cross-chunk and final-byte random reads. A
ninth capture still returns Busy while the eight pins and views remain
alive at the existing verified observation. After release, all eight
slots are reacquired together with the expected full identity, then
dropped before account verification or backup.

The complete pinned static-musl command passed all four Rust/native/RSS
observer cases and all four guarded worker-stack cases with unchanged
phase counts, quiet Rust controls and resource ceilings. Existing
backup/epoch source preservation, independently reopened
physical/account checks and destination-only epoch renewal remain.
RESOURCES.md and td-crypto/PORTABLE.md record actual measurements and
artifact inputs.

This qualifies simultaneous loan lifetime, interleaved progress and slot
reuse for this healthy same-body/same-account fixture on one process or
guarded worker. It does not qualify parallel threads, writes while
readers are retained, different accounts/bodies, arbitrary-account or
maximum-database work, numeric frame peaks, transient RSS, full
filesystem faults, power loss or whole-worker/service overlap. Earlier
maximum-pool records qualify owners with one borrowed view at a time and
remain tied to their own artifact/source hashes.

The earlier maximum-pool-only portable SQLite body/account/backup/epoch
qualification requested the supported maximum eight readers plus one
writer at creation and every reopen. The source and destination stores
in backup/epoch overlap with eighteen native owners through the final
verification observations. The one-reader native warmup before baseline
is unchanged. This qualifies full owner pools for the fixed 32 MiB body
and mailbox dataset, not eight simultaneously borrowed views or
concurrent reader work.

The complete pinned static-musl qualification passed all four
Rust/native/RSS observer cases and all four guarded worker-stack cases
without changing any resource ceiling. Backup/epoch retain independently
reopened physical/account checks, public copy and original source
preservation; epoch delegates one actual warmed 16-byte entropy fill and
persists the destination-only identity change through checkpoint/reopen.
RESOURCES.md and td-crypto/PORTABLE.md record the measurements, scope
and exact artifact inputs.

This qualifies these healthy bounded fixtures and their owner
configuration. It does not establish arbitrary-account or maximum-
database behavior, simultaneous borrowed-reader/body execution, numeric
frame high-water, a transient RSS bound, full filesystem faults, power
loss, complete worker composition or service overlap. Earlier portable
records remain tied to their own artifact/source hashes and used two
readers plus a writer per measured store.

The separate portable --sqlite-body-stack, --sqlite-account-stack,
--sqlite-backup-stack and --sqlite-epoch-stack cases passed their shared
32 MiB fixtures on one worker per fresh process. Each requests a 240 KiB
stack and verifies a non-growing writable mapping no larger than 256 KiB,
with an adjacent inaccessible guard of at least 4 KiB. Mapping checks
before and after the complete fixture agree; expected phase counts and
explicit worker joins precede completion. The body/account paths retain
their failed-source, oversized-body and ID-reuse controls; backup/epoch
paths retain real copy, reopen, physical/account verification and original
source checks. Epoch renewal uses actual warmed entropy on that worker.
RESOURCES.md and td-crypto/PORTABLE.md record the observed mappings and
artifact inputs. This is bounded fixture success on the fixed mapping,
not a measured frame high-water mark, arbitrary-account, maximum-database,
complete-worker, fault, power-loss or service-activation qualification.

A live borrowed view or body prevents consuming the engine. Every
returned error also consumes it, so an indeterminate commit cannot leave
a reusable owner with a stale epoch. The caller retains the root lock
and may reopen to inspect the persisted result; reopening alone grants
no service activation. Connection destruction retains its existing
synchronous cleanup limits. Authorization, stopped service activity,
resource admission, backup selection, digest/domain verification and
compatible configuration/credentials remain caller responsibilities.
Online backup, operational restore commands, verification/repair and
history maintenance tools remain unimplemented. SQLite integrity checks
do not replace digest and domain validation.

### Disposable ingress staging

IngressSpool borrows a separate LockedRoot exclusively. This root contains
only LOCK and slot-00 through slot-63; it must never be the authoritative
SQLite root. One owner validates the complete namespace before startup
cleanup removes any file. Inspection visits at most 65 entries, requires
LOCK and rejects unexpected names, directories, symlinks, extra hard links,
wrong owners, public/executable/special permissions or files above 32 MiB.
Cleanup accepts owner-only nonexecuting permission subsets of 0600 because
creation may die before restoring umask-filtered owner bits. Live completed
files require exact mode 0600. Startup inspects all 64 possible slots using
the hard size cap, even when configuration reduces current capacity.

Capacity is derived from the validated ResourcePlan: smtp_sessions plus
https_connections slots, each reserving the full configured message_bytes.
Defaults provide 16 slots and a 512 MiB aggregate logical byte ceiling;
compiled concurrency and message bounds permit at most 64 slots and 2 GiB.
The memory plan can refuse configurations below those theoretical maxima.
Event subscriptions do not permanently subtract HTTPS ingress capacity.
No file is created before its entire reservation is acquired. Fixed atomic
bitmaps retain occupancy and retirement; admission attempts at most 64
compare/exchange turns before Busy. Status is observational, not a coherent
cross-thread admission snapshot. Partial and completed files retain their
full charge until cleanup; no byte-growth counter or second body quota is
introduced. This reserves logical bytes, not filesystem blocks or free space.

SpoolWriter creates a slot name exclusively, restores mode 0600 and accepts
at most 64 KiB per write, refusing arithmetic overflow or the configured
message bound before I/O. It hashes successfully written bytes with the
existing Crypto provider. Write or digest failure is terminal, including
partial filesystem writes. Finish verifies descriptor identity, exact length
and physical EOF, finalizes the digest, checks the original clock/deadline and
rewinds before yielding an owned SpoolInput. It performs no fsync or durable
publication. Input retains its File and reservation, exposes passive account,
blob ID, kind, length and digest metadata, and implements bounded Read for
BlobSource. SQLite independently checks expected length, EOF and digest in
its atomic body/metadata transaction. No network read occurs through this
prepared input. File reads and rewind preserve the original deadline and
terminal errors; failure exposes the original typed error even when Read
reports an I/O kind. Synchronous kernel I/O cannot promise deadline
interruption.

Explicit discard reports cleanup failure; Drop attempts the same cleanup.
Close/take the File before unlinking, and return slot credit only after the
known owned inode is successfully unlinked. Cleanup bypasses expired work
budgets because it cannot undo acknowledged durable effects. A failure that
prevents proven owned cleanup conservatively retains and retires the entire
slot reservation. An unproven creation releases its slot only if a fresh
path lookup establishes NotFound after closing any file; all existing or
uninspectable paths retain the charge. Creation or identity failures never
justify removing an existing or unidentified inode merely to regain capacity.
In particular a
created file whose identity could not be established is left for startup
inspection. A retired slot stays charged until a fresh, quiescent startup under LOCK successfully cleans the namespace.
No temporary operation calls fsync; a crash may leave any subset of these
unacknowledged files, which startup discards. Process-death tests prove the
kernel releases the lock and the next owner removes the abandoned input.

Only successful SQLite COMMIT can authorize acknowledgement. Discarding an
input after that commit cannot remove authoritative bytes. A coordinator
chooses retry responsibility after rejection or indeterminate commit; the
spool grants neither account authorization nor a durable acceptance receipt.
The future ports::Store/BlobWriter/StagedBlob adapter, shared service quota
coordination, listener activation and configured root provisioning remain
separate. The low-level spool does not implement those ports or a CLI.

## 3. Metadata records

Each domain has an explicit table and typed columns, with account-scoped
primary keys and native foreign keys. IDs use exact 16-byte BLOB columns;
ordinary metadata is TEXT or INTEGER, not an opaque encoded value. Logical
ReadView key ordering remains FORMAT.md's unsigned byte order. Anchor/import
length-rank indexes preserve its little-endian length-prefix order without
storing encoded shadow keys. No table is loaded in full into RAM.

| Table | Key | Authoritative value |
| --- | --- | --- |
| `blobs` | blob ID | Kind (message/upload), length, SHA-256, creation time |
| `blob_chunks` | blob ID + chunk ordinal | Immutable body bytes, at most 64 KiB per row |
| `mailboxes` | mailbox ID | Name, parent ID, role, sort order, subscription state |
| `emails` | email ID | Message blob ID, thread ID, receivedAt, SMTP receipt/envelope metadata |
| `memberships` | email ID + mailbox ID | Empty; presence means membership |
| `keywords` | email ID + keyword bytes | Empty; presence means set |
| `threads` | thread ID | Persisted immutable grouping identity |
| `thread-anchors` | Length-prefixed Message-ID + email ID | Empty; authoritative lookup from message header ID to live email |
| `submissions` | submission ID | Email/thread/identity IDs, immutable transmitted blob ID, envelope sender, sendAt, lifecycle/notification state |
| `recipients` | submission ID + recipient ordinal | Address, attempt/phase, result, retry time, uncertainty, bounded diagnostic |
| `leases` | upload blob ID | Owning account/device, expiry and permitted use |
| `imports` | source-instance ID + source object kind + length-prefixed source-account and object bytes | Local IDs and verified source digest/mapping |

SMTP receipt recipients occupy smtp_receipt_recipients(account,email_id,ordinal,
address), preserving accepted order and duplicates. Email replacement updates
these children atomically; their foreign key cascades on email deletion. Other
owning foreign keys are deferred NO ACTION so caller batches explicitly remove
relationships and remain order-independent. Historical submission/import IDs
have no foreign key. Every domain row carries its last changed sequence. Submission expiry
is shared by its recipients; the exact positional fields
and enum tags are in FORMAT.md section 6. Fields needed for
submission remain in its record even if the visible email is later deleted.
Recipient ordinal is a big-endian u32 in the key so its byte order is numeric;
ordinary integer values use FORMAT.md section 1. Indexable timestamps and
addresses do not become primary keys solely for query speed.

The source account/object part of an import key, including its two u32 length
prefixes, is at most 1007 bytes; instance and kind take 17 bytes, giving a
total key ceiling of 1024 bytes. The kind distinguishes a mailbox and email
with the same source ID; source IDs are not globally unique across JMAP types.
Import must report an unrepresentable source ID; hashing without a
collision-resolving source record is not a substitute.
Keywords and addresses obey their more specific protocol/config limits.

Live owning references must resolve within the same account: email to
message blob/thread, membership to email/mailbox, keyword to email, recipient
to submission, submission to transmitted blob, and every lease to upload blob.
Mailbox parents must resolve without cycles. Submission email/thread/identity
IDs are historical identifiers: creation validates them and authorization,
but later deletion or configuration changes need not leave their targets live.
Frozen transmission bytes and envelope fields remain authoritative for sending.
Import mappings likewise retain historical local IDs after deletion; inspection
and resumed import report a deleted target instead of dereferencing it or
silently recreating it. Historical IDs never pin their former targets or grant
authorization. Lease device IDs retain provenance; revocation prevents use.
Thread anchors must reference live emails and are removed with their email.

`mailbox_parents::ParentWalk` checks one mailbox's parent chain through a
caller-owned ReadView. Capture the full view identity and admit an explicit
maximum number of lookups. Each advance performs at most one get, verifies the
view identity before and after it, validates the returned mailbox row and
sequence ceiling, then copies only its parent ID. Check the post-get identity
before interpreting a found row, absence or an error. ChangedView takes
precedence when the view moved. A missing row, invalid
row, changed view, lookup error or exhausted read budget retires the walk. Budget
exhaustion is a resource refusal, never proof of a cycle or a valid chain.

The walker uses Brent's cycle detection: advance one current ID and compare
against a saved ID replaced at power-of-two intervals. Reaching a root completes
the chain. A valid chain containing N mailboxes, including the root, needs
exactly N gets. At most 3*N gets suffice to detect a cycle among N reachable
mailboxes; callers deriving a budget from a mailbox count must use checked
arithmetic. Smaller admitted budgets may refuse a valid store. These are
lookup counts, not physical-read or time bounds, and this helper does not
enforce the separate configured mailbox-depth policy.
Direct self-parent rows already fail local row validation. This uses fixed
state rather than a growing visited-ID set. Completion reports the captured
identity, start ID and lookup count. It owns no pin and validates no other
mailbox chain or cross-row invariant. The coordinator must check every final
mailbox under the same actual view, preserve lookup failures as failures, and
retain its pins. The view owns each get's full work/scratch/deadline contract;
the caller also checks deadlines between advances. An advance after completion
performs no get, but still refuses a changed view. Failed or unfinished walks
cannot produce completion.

`mailbox_sweep::Sweep` enumerates every supplied final mailbox and runs
ParentWalk from each ID before advancing enumeration. Each advance performs
one next or one get, never a whole chain. Validate local source encoding,
sequence and strictly increasing mailbox IDs before admitting another row.
Check full captured identity before/after enumeration and through each get;
movement always reports the same top-level ChangedView. A finite row allowance
limits completed mailboxes and a separate total-get allowance spans all walks.
Give each new walk only the remaining allowance; exhausted work refuses rather
than declaring a cycle or a valid forest. Empty views need neither allowance.

Retain only one walker, previous mailbox ID, identity and scalar counts. No
visited forest or chain cache is built. Each walk re-reads its starting row,
even though enumeration already validated it. Total gets are the sum of every
chain length including its start and root: N root mailboxes need N gets.
Shared ancestors are read again, and a long chain can require quadratic total
gets across its starting mailboxes.
The caller admits those logical lookup counts and each lookup's full physical
work and deadline. Completed chains report their own lookup count; only EOF
after all chains rooted yields CompleteForest with identity, mailbox and total
get counts. Every error retires the sweep; incomplete/failed state cannot finish.
Repeated completion checks identity without I/O. This covers enumerated chains
only: physical enumeration completeness, actual pins, configured depth policy
and other graph invariants remain separate requirements.

`row_references::ReferenceCheck` validates direct owning references of one
supplied final row. Validate the source key/value and sequence ceiling first,
then retain only the borrowed source key and at most two copied typed targets.
Each advance performs at most one get through the supplied ReadView, checks
exact identity before and after it, validates the target row/key and sequence,
and requires the expected blob kind or recipient ordinal below the owning
submission's recipient_count. View movement takes precedence over a returned
row, absence or lookup error. Missing targets, invalid kinds/counts, malformed
rows, future sequences and lookup failures retire the whole check.

Email requires a message blob and thread; membership requires email/mailbox;
keyword and thread anchor require email; submission requires its transmitted
message blob; recipient requires submission; a mailbox's immediate parent must
exist. This direct check does not detect parent cycles; ParentWalk handles the
full chain separately. Submission email/thread/identity/notification IDs and
import mappings remain historical and cause no lookup. Blob and thread rows
have no outgoing owning references.

A lease must name the view's account and retain its upload target until the
lease is explicitly removed. The validation completion retains the trusted
UTC sample, but ownership is independent of expiration. Device authorization,
revocation and unexpired-use checks remain separate. CompleteReferences grants
no proof of body integrity, source-row custody or complete database validation;
the coordinator composes these checks before activation.

Source key bytes must remain valid until completion; target lookup uses a
separate caller result buffer and releases borrowed target rows before the next
advance. ReadView::next ties the returned Record's key and value lifetimes
together. To reuse that value buffer for reference lookups, copy/re-derive the
source key into independent cursor/key scratch or separately charged stack
storage and decode from that separate borrow before creating ReferenceCheck.
The helper does not detach a Record's shared borrow or obtain another arena
implicitly. A zero-target check still verifies identity on its first advance.
Completed checks perform no more lookups but continue rejecting identity change;
failed/unfinished checks cannot finish. At most two gets are performed, each
subject to the view's work/scratch limits and caller deadline admission.

`reference_sweep::Sweep` enumerates the supplied ReadView's final rows in all
11 table kinds and runs ReferenceCheck for each. One advance performs one next
and at most two gets. Validate view identity before and after next, with changed
view taking precedence over row/absence/error; then require the requested table,
strict canonical key progression, local source validity and sequence ceiling.
Copy the source key into a fixed independent key buffer and decode it there,
releasing next's shared key/value borrow before reusing the value output for
target lookups. No source row or key borrow survives the advance.

Local source/table/order/sequence errors precede resource refusal. A found row
beyond the caller's finite max_rows limit then refuses before target lookups.
Count only fully checked rows; checked counters and fixed per-table counts
cannot wrap. An exhausted next reports TableComplete and moves to the
next table on the following advance. After all tables, a separate identity-
checked advance reports Complete. Zero allowed rows can still prove all tables
empty. Failures permanently retire the sweep and forbid finish; repeated
Complete performs no lookups but still rejects changed identity.

Completion retains captured identity, the single UTC sample, total and per-table
row counts. It proves direct checks over rows returned by the supplied view,
not that the view enumerated physical files completely. Selected-file integrity,
actual pins, blob byte verification, every mailbox parent chain and aggregate
rules such as missing recipient rows remain separate coordinator work. Every
step needs admission for the view's one-next-plus-two-get work and a deadline;
these are logical lookup bounds, not a physical I/O or time bound. Identity
movement during either next or a target get reports the same top-level
ChangedView error; other reference failures retain their nested classification.

`recipient_sweep::Sweep` verifies FORMAT's exact recipient coverage over the
supplied view. Walk ordered submissions and recipients with one next per
advance. For each locally valid submission, copy its ID/count and require
recipient ordinals exactly 0 through count-1 for that ID. Read the next
submission only after the current group is complete. Never skip forward to a
matching recipient: a later group/ordinal or early EOF reports the missing
expected ordinal; an earlier group or rows after all submissions reports an
unexpected recipient. Require strict order in both streams independently.

Check full identity before/after each next, with movement preceding all results.
Validate source table/key/value and sequence ceiling, copy only scalar IDs/counts
and release all row strings before returning. A finite total-row allowance covers
both streams. Local corruption and coverage errors precede row-budget refusal;
a valid row beyond admission refuses before advancing progress. EOF remains
checkable at the exact allowance, including an empty view at zero rows.
Complete requires EOF on both streams after every exact group. It retains the
captured identity and submission/recipient counts. Errors retire the checker;
failed/unfinished state cannot finish, and repeated completion still checks view
identity without another read.

Each recipient also obeys QUEUE.md's current-state phase, retry-presence,
uncertainty and failure-reason rules. InFlight/Body and
InFlight/AcceptancePossible require a positive RCPT reply and absent DATA
reply, while Prepared may retain earlier replies. The same rule applies to
recovery and final groups in typed/encoded commits, including newly created
rows. Accepted requires positive RCPT and final DATA reply codes; definitive
SMTP failure/retry requires the applicable stored negative code. Unattempted
recipients cannot carry replies. Code/separator checks do not replace wire
parsing or full JMAP reply normalization. Historical
replies alone never authorize an attempt result or prove its fence.

For each exact group, completedAt must be present exactly when no recipient
has a future dispatch obligation. Retryable OutcomeUnknown remains pending;
terminal OutcomeUnknown contributes a failure requiring a Pending/Stored notice.
Unknown is terminal exactly for Expired/SmtpPermanent; other allowed reasons
must retain a next attempt.
A notice is required for a completed group containing Failed or OutcomeUnknown.
A completed, wholly Canceled group may retain an earlier Pending/Stored failure
notice; cancellation alone creates no notice. Remaining groups require None. Cancellation is
submission-wide: a group cannot mix Canceled with other states. Only scalar
flags are retained; completedAt is not compared with sendAt because a wall-clock
step can put completion earlier. Queue errors carry the submission and an
optional recipient ordinal; a group error appears at its final recipient.

CompleteCoverage establishes these current-row/group rules, not physical view
completeness, direct owning references, transition history, worker fencing,
creation authorization or actual pins. The caller composes those validations
under the same immutable view. Each step needs admission and
deadline checks for the supplied view's full next operation; one logical lookup
is not a physical I/O/time limit. Fixed progress and 20-byte cursor scratch use
the worker stack; caller key/value result partitions are reused.

`metadata_sweep::Sweep` composes the direct-reference, mailbox-forest and
recipient-coverage sweeps in that order under one captured ViewIdentity
and trusted UTC sample. One advance performs one child turn, at most one
next and two gets, using the caller's key/value buffers. Limits.rows
bounds each pass independently; Limits.parent_reads bounds all parent
walk gets. The three passes can enumerate up to three times the row
allowance, with up to twice that allowance in direct-reference gets plus
the separate parent-get allowance. These are logical lookup bounds;
callers still admit each lookup's full physical work and deadline.

Only one active child and fixed completion receipts are retained. Child
failures retire the entire composition. View movement is reported as one
top-level ChangedView, including movement accompanying lookup failure.
Before completion, compare mailbox, submission and recipient counts from
the reference pass with the corresponding later-pass counts. Mismatch
refuses; failed or unfinished state cannot finish. The final advance and
repeated completion check identity without I/O. Completion retains all
three original receipts, including the UTC sample and parent-get count.
This checks supplied rows and queue groups, not physical enumeration
completeness, body digests/extents, anchor cardinality, historical
transitions, authorization or pins. Actual snapshot custody and the
remaining offline verification/activation checks belong to the caller.

Thread assignment does not depend on disposable indexes or rescanning every
body. Store at most one anchor per email: its first syntactically valid
Message-ID within a 1004-byte ceiling (four-byte length plus ID plus 16-byte
email ID fits the key limit). Header ID matching is byte-exact after parsing
away delimiters and grammatical CFWS. Absent or oversize IDs produce no anchor;
raw headers remain unchanged. Examine at most the last 32 References IDs,
nearest first, then bounded In-Reply-To IDs, then the email's own anchor ID.
Use the first ID with a live
anchor; duplicate anchors select the lexicographically smallest email ID.
Join that email's persisted thread. If no candidate resolves, create a fresh
thread. Existing email/thread assignments are immutable, even when a later
arrival connects two conversations. POLICY.md section 5 pins malformed and
repeated-field behavior; CASES.md T01-T04 pin the fixture outcomes.
Resource/I/O failure is an explicit temporary error, not permission
to silently choose a different thread. Authoritative anchor lookup remains
available without a cache, using bounded-work indexed SQLite access.

IndexReadView::thread_anchor resolves one already parsed Message-ID in its
captured account snapshot. The input must contain 1..1004 UTF-8 bytes;
header syntax, CFWS removal and candidate ordering remain caller work.
The caller skips overlength IDs under POLICY.md section 5 before calling
this helper; they retain their place in the References lookback limit.
The native primary-key lookup uses exact message_id equality and selects
the smallest unsigned Email ID, including the all-zero ID, with LIMIT 1.
It neither case-folds nor normalizes the supplied ID. No match returns
None; it does not allocate a new thread or fall back to another candidate.

The selected anchor's changed sequence must not exceed the view endpoint.
The resolver then uses the existing typed point reads for its Email and
Thread, validating their rows and changed sequences. A missing or corrupt
selected target returns Corrupt instead of selecting a different Email.
One indexed anchor lookup and two logical point reads share the original
read transaction, deadline and VM allowance; an Email's bounded SMTP
receipt decoding can perform additional native child-row work. Caller
value scratch holds the Email row; format::MAX_VALUE_BYTES (65536 bytes)
holds every valid row. SMTP receipt lists have their own 32768-byte and
1000-recipient ceilings. Insufficient scratch returns Capacity.
Errors never mean no match. Returned Email/Thread IDs are passive metadata,
not body pins or write authority. Lost snapshots and native budget failures
retain the read view's existing sticky failure behavior. This native helper
does not implement header selection, creation-frame ordering or the future
authenticated Store adapter.

The native committing writer checks each surviving ThreadAnchor PUT after
row writes against the final account view. An indexed lookup using
anchors_email(account,email_id) tests for a second anchor with LIMIT 1
OFFSET 1, visiting at most two matching entries. A second anchor returns
Conflict and rolls back metadata and bodies together; a prepared body
source may already have been consumed. Up to 4096 such lookups share the
original transaction deadline and VM allowance, including repeated PUTs.
Intermediate duplicate anchors repaired by final deletion are allowed.
One Message-ID may still anchor several different Emails, and the same
Email ID in another account does not contribute to this check.

This preserves the per-Email cardinality rule from an initially
consistent store: any new duplicate requires a surviving anchor PUT. It
is not a whole-store integrity scan. The separate validate_integrity
operation checks this cardinality across every account after SQLite
integrity_check(1) and foreign_key_check. The full physical check
verifies index contents against their tables before any domain scan
trusts an index; quick_check's entry counts alone do not establish that
agreement. It stops after the first physical error. Its table/index
comparisons may require O(N log N) work, bounded by the original
maintenance deadline and finite VM allowance. One ordered covering scan
of anchors_email groups by account and Email ID and refuses a group with
more than one row as Corrupt. The scan needs no temporary grouping tree
or body reads. It shares the original writer fence, maintenance deadline
and finite VM allowance; resource or I/O refusal cannot grant
completion. This scan is explicit maintenance and does not run during
open or ordinary commits. Selecting the first valid header ID, proving
anchor/body correspondence and authorizing anchor changes remain service
obligations. Deferred owning foreign keys require anchor removal with
Email deletion.

A committing writer validates these rules, blob kinds, keyword limits and
submission/blob pins. Derived counters are calculated or cached, never
independently authoritative. A transaction that updates folder membership also
declares the affected JMAP objects/state types for change APIs.

For example, `store inspect email e123 --json` might decode:

```json
{
  "emailId": "e123",
  "blobId": "b91",
  "threadId": "t55",
  "receivedAt": "2026-09-22T18:30:00Z",
  "mailboxIds": ["m1", "m7"],
  "keywords": ["$seen"],
  "viewSequence": 812
}
```

Inspection object-ID fields use the type-prefixed wire form from WIRE.md;
the shortened IDs in these worked examples are schematic, not valid inputs.
Physical primary keys use explicit domain columns; FORMAT.md keys are the
transient caller representation.
That JSON is an assembled inspection view, not a JSON file on disk. Its fields
come from the email, membership and keyword rows in the captured SQLite snapshot. `m7`'s name comes from its mailbox row. Subject and attachment
names come from the message or its disposable parsing cache.

### 3.1 MIME part blob identities

Stored blob IDs and JMAP part blob IDs are distinct typed forms. A part ID is
a versioned encoding of its parent stored blob ID, encoded-body offset/length
and transfer-encoding tag; [WIRE.md](WIRE.md) freezes its canonical bounded
wire encoding within JMAP's ID length limit. Nested attached messages use its
bounded chain of decoded-stream ranges. A part never names an independently
stored body or an entry in `blobs`. Resolve it only in an authorized account and live parent
view, or against an authorized unexpired upload lease for a parsed raw message;
validate checked ranges and require an exact match to a parsed MIME part
descriptor, rebuilt boundedly if its cache is absent. A forged locator cannot
select arbitrary body bytes or bypass parent authorization.

Download streams transfer-decoded part contents from the immutable parent;
unknown transfer encodings follow the JMAP identity-decoding rule. A read view
pins the parent for the entire stream. Email/set attachment reuse resolves and
pins the parent while assembling the new immutable message, then commits its
own body; the new email must not depend on the original surviving. Part IDs
do not keep parents alive after all ordinary references expire. No decoded
attachment file or cache is authoritative. Parser/schema upgrades preserve
existing locator semantics or require an explicit format migration.

## 4. Acceptance boundary

Storage tests exercise actual SQLite snapshots, reopen, unsigned sequences,
rollback, owning references, parent cycles, atomic body writes and snapshot reads after deletion.
Native allocation/RSS qualification and complete crash/fault matrices remain
required before service activation. Rust allocation evidence for pure MIME
processing does not qualify SQLite or whole-service memory.

Opening checks fixed schema/header state without a full-database scan. Full
integrity validation is explicit bounded maintenance. Runtime commits use indexed deferred foreign-key enforcement instead of scanning
all accounts. The writer preallocates 128 KiB row/reference scratch at cold
startup; 64 KiB body chunks reuse it. Core blob metadata cannot admit a body
above 32 MiB; caller policy enforces any lower message/upload ceiling.
Writer fence acquisition refuses immediately when occupied.

The ignored `large_wal_checkpoint_fits_native_allocation_cap` fixture is an
explicit large-WAL qualification. It keeps all nine permitted native
connections, retains an old snapshot while replacing one 32 MiB body per
commit, then drops the snapshot and truncates the WAL. It bounds generation
at 160 commits (5 GiB input), a 6 GiB WAL extent and 15 minutes checked
between calls; native calls remain synchronous. It verifies the preserved
snapshot, checkpoint refusal while borrowed, successful truncation, a
sub-64-MiB database, and reopened sequence/integrity/body contents.
Ordinary gates leave this multi-gigabyte fixture ignored.

Build through the normal forced native driver with an optimized test profile
so hashing does not dominate the qualification:

```text
CARGO_PROFILE_TEST_OPT_LEVEL=2 target/release/td-builder gate-crates crypto-cargo test --manifest-path td-mta/Cargo.toml
```

This command runs the ordinary mail suite to produce its test executables.
Use the `td_mta-...` path on its `unittests src/lib.rs` line, not the
`src/main.rs` executable, to set `td_mta_lib_test` below. Cargo prints a
repository-relative path; run from the repository root. Select an existing
private, disk-backed TMPDIR with at least 6 GiB free; a tmpfs consumes RAM
outside the test process's RLIMIT_DATA. Apply an outer timeout for native calls:

```text
td_mta_lib_test=.td-build-cache/crypto-target/x86_64-unknown-linux-gnu/debug/deps/td_mta-LIB_TEST_HASH
TMPDIR=/path/on/disk timeout --kill-after=5s 1200 target/release/td-builder run-capped "$td_mta_lib_test" --ignored --exact store_fs::index::tests::large_wal_checkpoint_fits_native_allocation_cap --nocapture --test-threads=1
```

Require exit status zero and a libtest summary of exactly one passed test
and zero failures. Zero matched tests is a qualification failure. The fixture
prints its private root. If external termination prevents Drop cleanup, first
confirm that invocation and its descendants have exited, then remove only
that printed root; never sweep other agents' temporary directories.

The x86-64 GNU host run used rustc 1.99.0-nightly (6f72b5dd5), test
opt-level 2 and Linux 7.0.14/btrfs. It generated 1050433 valid frames
in a 4327783992-byte WAL. The original 2 MiB individual cap refused checkpoint
with Capacity. With the 9 MiB cap the checkpoint took about two seconds;
the complete fixture, including WAL generation and reopen/content checks,
passed in 83.14 seconds with the shared 16 MiB limit unchanged. This qualifies that
specific failure boundary, not maximum-WAL mapped memory, native stack/RSS,
the complete 8 GiB database, or combined service overlap.

The separate ignored `maximum_wal_crash_recovery_and_checkpoint` fixture
qualifies the near-ceiling WAL path. Its test-only producer bypasses normal
`reserve_wal` admission so it can reach the physical limit with a small
database: it commits a valid 32 MiB body, checkpoints, holds an old read
snapshot, then alternates changed 64 KiB rows in separate commits using one
bounded UPDATE statement per row. It finishes with a metadata commit and
parks all nine native connections. The producer checks the 34 MiB SHM file
bound. The parent kills that exact child with SIGKILL, checks the complete
aligned WAL, and reopens the store. Recovery occurs while the first
connection opens; all eight readers plus the writer are present for the
subsequent TRUNCATE checkpoint. The parent verifies the final metadata
commit and full body through the recovered WAL, then checks the zero-length
WAL, integrity, metadata and body after checkpoint. The test also checks
the 8 GiB database bound, but its database is only about
32 MiB. That small-database fixture does not qualify an 8 GiB database
checkpoint; the separate maximum-database fixture below covers its own
body-dominated case. Normal reservation scheduling remains unqualified.
The producer changes normally immutable body chunks and
`created_at` without advancing the account sequence; its final state is
valid for this physical WAL oracle but cannot come from the public API.
The snapshot begins at an empty WAL. With automatic checkpointing disabled,
the test does not qualify a reader pinned inside a large WAL.

After the optimized build above, run this fixture explicitly with the same
`td_mta_lib_test` executable. Use a private disk-backed TMPDIR with at least
40 GiB free and an outer 45-minute timeout; ordinary gates leave it ignored.
The resource observations require Linux `/proc/self/smaps_rollup` and
`/proc/self/status`; missing probes fail the qualification:

```text
TMPDIR=/path/on/disk timeout --kill-after=5s 2700 target/release/td-builder run-capped "$td_mta_lib_test" --ignored --exact store_fs::index::wal_qualification::maximum_wal_crash_recovery_and_checkpoint --nocapture --test-threads=1
```

Require exit status zero and exactly one passed test. If external termination
prevents cleanup, confirm that invocation and its descendants have exited
before removing only its printed private root. On the x86-64 GNU host with
rustc 1.99.0-nightly (6f72b5dd5), Linux 7.0.14 and btrfs, the test wrote
4194368 valid frames in a 17280796192-byte WAL, the largest whole-frame
WAL under the 17280796224-byte admission ceiling (a 32-byte gap).
Recovery open took 21.01 seconds, the
pre-checkpoint read through the recovered WAL took 0.535 seconds, and the
checkpoint took 10.94 seconds; the complete test passed in 222.03 seconds.
The 9 MiB individual and 16 MiB process-wide SQLite requested-allocation
caps were unchanged. The largest reported child `VmHWM` sample was
39480 KiB; the parent reported 48244 KiB after checkpoint. These are
separate-process observations, not a whole-service memory peak or a bound
on transient RSS.

The separate ignored maximum_database_checkpoint_and_account_maintenance
fixture fills the actual 8 GiB main database through public streamed
Blob commits, checkpointing during admission and halving body sizes
after clean Capacity refusals. It bounds admission at 512 attempts and
retains only IDs and fixed-size expected rows. If public admission stops
within 128 pages of the cap, a bounded test-only CREATE/INSERT/DROP
padding table may leave free pages under auto_vacuum NONE; the completed
database must still have the exact fixed schema on reopen. The recorded
run reached the cap through public commits alone; it did not exercise
the padding fallback.

After asserting exactly 2097152 pages and 8589934592 main-file bytes, a
test-only producer temporarily changes and restores each 64 KiB chunk
before moving on, committing once per body. It preserves the original
rows, digests, epoch and account sequence while bypassing normal WAL
reservation scheduling. Require an aligned WAL of at least 8 GiB and at
most the admitted WAL ceiling, plus the SHM bound. Keep all eight
readers and the writer present for TRUNCATE checkpoint, require zero WAL
length afterward and the unchanged main-file extent. This exercises a
large dirty body dataset in a full database, not a claim that every
main-file page is copied.

Physical integrity and complete account verification then run before and
after reopening with eight readers. The oracle compares every accepted
typed Blob row and changed sequence, the epoch, account endpoint/history
floor, permanent-ID count and complete metadata/body report counts and
declared bytes. All bodies finish with their original digests. Account
verification reuses one caller-owned 64 KiB buffer and the captured
maintenance view allowance, without per-body renewal. The fixture uses a
fixed clock and a 30-minute wall bound checked between calls; apply the
outer 45-minute timeout for synchronous native calls.

After the optimized native build above, use a private disk-backed TMPDIR
with at least 40 GiB free and invoke the library test executable
explicitly:

```text
TMPDIR=/path/on/disk timeout --kill-after=5s 2700 target/release/td-builder run-capped "$td_mta_lib_test" --ignored --exact store_fs::index::database_qualification::maximum_database_checkpoint_and_account_maintenance --nocapture --test-threads=1
```

Require exit status zero and exactly one passed test. Ordinary gates
leave this disk-heavy case ignored. The fixture prints its private root;
after external termination, confirm the invocation and all descendants
have exited before removing only that root.

The 2026-10-09 x86-64 GNU host run used rustc 1.99.0-nightly
(6f72b5dd5), test opt-level 2, Linux 7.0.14 and btrfs. Public admission
accepted 262 bodies and returned 16 clean Capacity refusals, reaching
the exact 8 GiB database without padding. It produced an 8640991392-byte
WAL and checkpointed in 18.874 seconds. Physical scans took 9.837 and
9.770 seconds; complete account passes checked 8513712128 declared body
bytes in 58.278 and 58.017 seconds, before and after reopen. The libtest
summary passed exactly one test in 697.74 seconds with zero failures.
The native library test executable SHA-256 was:

```text
0dd8f38dd9b654f7a1b3fbb6b5254f6f4f3046daf8fa3d4598e2722ad7c736f2
```

The 9 MiB individual and 16 MiB shared SQLite requested-allocation caps
were unchanged. RSS samples were 5644 KiB at baseline, 11356 filled,
27652 dirty and checkpointed, 28240 after verification and 11960 after
reopened verification. The largest reported VmHWM value was 31024 KiB.
These host observations qualify this body-dominated dataset and native
checkpoint/maintenance path; they do not qualify arbitrary-account
metadata, normal service WAL scheduling, wrapped allocator measurements,
guarded stack, transient RSS, power loss, full filesystem faults,
maximum backup or whole-service overlap.

Creation is exclusive but not crash-atomic. A failed initial creation can
leave an incomplete database or a complete durable database whose startup
validation or connection-pool preparation exceeded the deadline. Neither
create nor open overwrites it. With the service stopped and root exclusively
locked, first try opening it with a fresh startup scope. Inspect and remove
only an incomplete new database and its SQLite sidecars before retrying
creation.
Never apply this reset to a valid database with authoritative bodies.
The bundled SQLite compile retains upstream optional modules; closed runtime
queries expose none as an API. Runtime version admission requires 3.53.2.

Small body reads admit one bounded SQLite query per call and fetch at most two
64 KiB chunks, including for an unaligned request. SQLite may materialize a
complete chunk for a smaller returned slice; MIME byte meters count returned
logical bytes, not that native copying or B-tree work. Native VM/heap caps and
the original deadline apply separately. No per-pin chunk cache is reserved.
The WITHOUT ROWID chunk table favors one account/blob/ordinal key lookup;
its 64 KiB payloads may deepen native B-trees. This fixed per-call amplification
is admitted, not a claim of physical I/O equal to returned MIME bytes.

The separate IndexReadView::verify_bodies primitive enumerates all
returned Blob rows for one account snapshot and reuses PinnedBlobInput
for complete length, digest and extent verification. Caller-owned
64 KiB scratch is reused for every body; only fixed cursor/row buffers
and one digest/input owner are retained. Explicit blob-count and total
logical-byte limits are checked before opening each body, and every
metadata/body query retains the original view deadline and finite VM
fuel. No per-body allowance or fresh view is acquired. It is synchronous
maintenance and does not promise one-body scheduling or completion at
the maximum database size. Ordinary views retain their normal allowance;
explicit maintenance views retain their one larger finite allowance.
Neither kind renews fuel within verification.

CompleteBodies reports the captured ViewIdentity and counts only after
every enumerated body finishes and enumeration terminates successfully.
An empty account can complete with zero limits; any native, crypto,
clock, capacity, corruption or snapshot-loss failure yields no report.
The report is historical data, not a body pin, authentication credential
or ongoing freshness proof. Physical/index consistency and orphan chunk
absence require separate whole-store maintenance; metadata checks and
body reports must identify the same snapshot before an offline
coordinator combines them. This primitive does not enable the operational
verify/restore CLI or qualify complete crash/fault/resource behavior.

The pure account_checks::combine guard packages CompleteMetadata and
CompleteBodies only after equality of account, epoch, committed sequence
and history floor and matching declared Blob counts. An identity or
count mismatch refuses without constructing CompleteChecks. The
private-field result retains both historical reports and exposes them
read-only; it acquires no view, pin, clock, I/O or renewed allowance.
Matching historical WAL reports are admissible even after the current
writer advances, so the package is neither ongoing freshness nor body
custody or authority. Physical/index completeness and other full-store
verification remain separate offline-coordinator requirements.

IndexReadView::verify_account composes the metadata sweep, complete
body pass and report guard synchronously over the same captured view.
AccountCheckLimits supplies each existing metadata and body limit;
trusted metadata UTC comes from the caller. One caller-owned 64 KiB
scratch buffer serves metadata values before body streaming, beside a
fixed 1024-byte metadata key buffer and the inline metadata sweep.
Metadata failure stops before body work; later body or report failure
also yields no combined completion. Error variants retain their source.
The original deadline, snapshot and normal-or-maintenance allowance
cover every stage without renewal. Physical/index validation, whole-store
coverage, custody, authorization and operational activation remain
separate requirements; this method returns only historical CompleteChecks.

The account allocation/RSS probe covers a bounded three-row snapshot:
one 32 MiB body and a parent/child mailbox tree, checked through the
explicit maintenance view before and after reopen. Its fixed source,
digest oracle and streaming scratch do not retain the whole body.
RESOURCES.md records the measured scope and limits; this fixture does
not qualify maximum-account/database completion, complete filesystem
fault recovery, guarded stack or service activation.

Full validate_integrity maintenance refuses a stopped writer and holds
the writer fence throughout its scan. Existing read views remain usable;
new view capture and commits return Busy until it finishes. It reports
physical SQLite, foreign-key consistency, per-Email anchor cardinality
and complete blob chunk geometry, not body digests, full domain
consistency or protocol authorization.

After the physical, foreign-key and anchor checks, one global blob scan
uses two chunk primary-key probes scoped to each account/blob. The
stored count must equal ceil(length/65536), every ordinal must be in that
extent, and each chunk must have exactly min(65536, length-ordinal*65536)
bytes. Unique chunk keys plus count and ordinal bounds establish the
contiguous layout; an empty blob must have no chunks. Malformed geometry
returns Corrupt. No whole body is buffered or hashed by this query. It
shares the original maintenance fence, deadline and finite VM allowance;
resource, clock or I/O refusal cannot grant completion. Selected body
pins still perform their own length, digest and extent verification.
