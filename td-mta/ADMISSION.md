# Disk admission, bounded work and request retention

This is the normative companion to [RESOURCES.md](RESOURCES.md),
[API.md](API.md) and [STORAGE.md](STORAGE.md). M02c3b freezes the policies.
M04c1 implements checked disk/work configuration in
[src/admission.rs](src/admission.rs); it produces an immutable plan from an
already validated ResourcePlan, DiskLimits, WorkLimits and explicit
ViewMode. It validates capacity relationships without allocating pools or
inspecting the filesystem. M04c2 supplies pure charged meters and timer
budgets in `src/admission/work.rs` and `src/admission/timers.rs`. M04c3a
supplies pure physical-space arithmetic in `src/admission/space.rs`. M04c3b1
supplies fixed logical leases and linear effect tickets in
`src/admission/logical.rs`, using `src/admission/quota.rs`. M04c3b2 supplies
the scalar writer/checkpoint ledger in `src/admission/writer.rs`. M04c3b3
owns the composed physical reservation coordinator. Its first increment,
M04c3b3a, supplies bounded filesystem registration and linear probe matching
in `src/admission/filesystems.rs`. M04c3b3b composes atomic admission and effect
accounting in `src/admission/coordinator.rs`; checkpoint transfer/reopening
remain M04c3b3c. M05/M08 supply physical probes, persistence
and maintenance.
M13 owns request retention. No running admission coordinator or filesystem
probe is claimed.

## 1. Disk accounting

Disk lengths, offsets, counts and arithmetic use checked u64 values. Convert
only an individual bounded I/O chunk to usize. A four-GiB quota is not a
four-GiB allocation or an assumption that every target has 64-bit usize.
One coordinator serializes accounting and reservations for the store, grouped
by backing filesystem where logs or private scratch reside elsewhere.

The default personal profile has these ceilings. MiB and GiB are binary units.
These are caps on use, not files preallocated to every cap at startup.

| Account/service resource | Default ceiling and charge |
| --- | --- |
| Raw body files | 4 GiB of message/upload/published-orphan/private-body file lengths, plus unspent reservations |
| Body files | 250000 message/upload/orphan/private-body files, plus reserved new files |
| Live metadata | 256 MiB of projected checkpoint records including table headers |
| Checkpoint generations | 2 GiB total selected, retired and building files; at most one current, two pinned retired and one building generation |
| Advertised retained history | 128 MiB and 64 segments; seven-day retention is a target within both limits |
| All closed journals | (storage_views + 1) * (128 MiB + journal_bytes), initially 396 MiB; at most (storage_views + 1) * 65 segments, initially 195 |
| Active journal | 4 MiB and 8192 operations, including outstanding commit reservations |
| Upload lease quota | 128 MiB, charged once per distinct upload blob with an active lease |
| Queue quota | 256 MiB and 1000 retained submissions; charge each transmitted blob once while any retained submission pins it |
| External-sort scratch | 64 MiB across all runs; one sort job |
| Request retention | 128 MiB per request, 256 MiB across all active requests, including response indexes and creation-ID maps |
| Disposable disk caches | 128 MiB across current and rebuilding files |
| Logs | Five 8 MiB files, as derived by Limits::plan |
| Mutable cold state | 16 MiB for device/ACME state and old/new generations, excluding externally maintained configuration/secret source files |

Existing upload, queue, sort and log configuration limits retain the checked
ranges in limits.rs. M04 implements the other disk settings with defaults above
and finite compiled maxima: body bytes 1 TiB, body files 1000000, live metadata
1 GiB, checkpoint files 8 GiB, response retention 1 GiB/request and 4 GiB total,
caches 1 GiB, cold state 64 MiB. History, journal and generation-count maxima
remain fixed by FORMAT.md/STORAGE.md. Validate all relationships, including
per-request retention <= aggregate retention, raw body quota >= message_bytes,
and checkpoint quota >= four times (live metadata cap + journal_bytes + 1 MiB).
Per-request retention must be at least
`32 * json_bytes + 4096 * json_methods + 64 KiB`, a conservative bound for
mandatory framing/errors and escaped initial creation-map copies. Count actual
request reservations, not this entire startup ceiling, against aggregate use.
The live metadata cap must fit all empty table headers (1232 bytes), and
the cache cap must fit the 1 KiB GC cursor reserve. Other new byte/count
caps must be positive. Changing quotas does not enlarge
any RAM pool. Quota reductions below current
use fail configuration validation rather than delete existing data.
Logical upload/queue quotas are independent policy ceilings: they may be
smaller than message_bytes or larger than the raw-body quota. Effective
admission is the intersection of all applicable quotas and current use.
maxSizeUpload advertises a per-object ceiling, not a guarantee that remaining
account disk quota can hold such an upload. Redacted effective configuration
reports the overlap; do not silently raise operator quotas to match it.

Upload and queue quotas are logical subquotas of the raw-body namespace. Do
not add those body lengths a second time to physical use. A blob may consume
both logical categories while occupying one file. Expiry/removing its last
category pin releases that logical charge, but its raw-file charge lasts until
unlink. Failed writes and interrupted publication retain their written-byte
and file charges until safe cleanup; releasing an unused reservation cannot
make an orphan disappear from accounting. Reusing an existing blob still
reserves any new logical category charge before creating the reference.
Completed submissions retained by QUEUE.md still consume queue count/body
quota. Reaching that quota refuses new submissions even if none is currently
pending. Automatic eligible retention cleanup and explicit administrative
deletion reclaim the logical quota; neither may bypass QUEUE.md's minimum
retention or uncertainty acknowledgements. Status reports pending and retained
counts separately so this refusal is diagnosable.

Count new shard directories and temporary metadata/control files in the
filesystem inode reservation, even though they are not body files. All 256
shards may be prepared during startup; if created later their publication and
directory sync remain part of the admitted operation. A checkpoint's projected
live size is computed from the final row changes before commit, with checked
old/new record lengths. Tombstones do not let a live table exceed its quota.
Size-nonincreasing deletion remains possible at a full logical quota, subject
to journal, response and physical completion capacity.

The service reconstructs charges under LOCK before opening listeners, including
private/orphan files, every selected/unselected/retired/building generation,
history and caches. Nothing is made invisible merely by being unselected.
Disposable request/sort files from an earlier process are removed only from
validated private namespaces. Live body/checkpoint files are not scratch.
No mailbox-sized in-memory inventory is required; bounded scans and sort runs
perform reconciliation. Counters are admission aids, not on-disk authority.
After validating CURRENT, its manifest/journal and recovery boundary, startup
has no surviving process pins. It can then remove proven-unselected checkpoint
builds/generations and private unpublished body files in their recognized
temporary namespace, syncing removals before releasing charges. No authoritative
reference ever targets a private body pathname. Published message/upload orphans
require the separate exclusive inventory/liveness proof. Unknown entries are
reported, never swept by a broad tmp-directory deletion.

Journals are excluded from checkpoint-byte charges. Count each closed segment
once against the all-closed-journals cap, whether selected for history, pinned
by a retired view or both. Each view can pin its bounded history plus one
journal prefix; the separate cap permits all admitted views and current history
to coexist. Before a rollover, reserve the newly closed segment and next active
file. Prune only unselected/unpinned files after publishing a safe history floor;
defer checkpoint/admission if the cap cannot be met. A full advertised history
does not itself require evicting a pinned segment or stalling every rollover.

### Logical lease implementation

M04c3b1 keeps up to 64 caller-owned cells, with four logical quota pairs per
cell and at most eight cells in one atomic group. Duplicate kinds add across
the whole group; every applicable category must fit used plus pending plus
new charges before any group is installed. Constructor use comes from trusted
store reconciliation and cannot exceed configured caps. These counters are
not disk authority. The helper does not grant filesystem or writer permission;
M04c3b2/M04c3b3 must couple its reservation to the checkpoint and space gates.

Group and part tokens validate the complete process-local slot generation.
A bounded extension increases a part's reservation without allocating another
cell; the composed coordinator also requires fresh physical admission.
Initially zero amounts disable their positions for the lease lifetime; an
extension cannot activate them. An enabled position consumed to zero keeps
its kind and can be extended. Packed kinds and amounts avoid pair padding. A
linear effect ticket pins a part before work. A busy part cannot be extended,
reused or canceled. Proven completion consumes only its exact bounded charges;
an uncertain effect conservatively consumes the planned charges. Invalid
completion leaves the ticket and reservation pinned. Completion can run after
the lease deadline; expiration refuses new work and cannot undo effects.

Writing raw/private bytes and publishing logical references are separate
transitions with separate tickets/charges. A syscall error alone does not
prove zero growth. Cancel releases only unused reservations; used raw/orphan
charges remain. The logical helper has no public operation to release used
charges. M05/M08's object ledger and proven cleanup/commit transitions own
that later integration; a freed lease token cannot authenticate object cleanup.
M04c3b2/M04c3b3 wrap this same logical ledger, without duplicate quota-used
counters. Typed journal commit/rollover and object cleanup transitions must
account for all recycled buckets, including scratch and logs. Proven effect
amounts are trusted adapter inputs, not capabilities against arbitrary code.
The composed coordinator must restrict who can supply that proof.

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

### Writer ledger implementation

M04c3b2's WriterLedger owns the logical lease table, selected table-length
baseline and scalar writer phase. It reads committed journal frames from the
same ledger's used buckets and outstanding frames from its pending buckets;
there is no duplicate journal counter. Recovery supplies the selected lengths
and used quotas from one trusted snapshot. Active bytes exclude the 96-byte
segment header. Zero committed frame bytes and zero operations must agree.

Preparation checks all logical caps and derives the checkpoint bound from
selected tables plus committed, pending and candidate frame bytes/operations.
A prepared request holds an exclusive ledger borrow through the future physical
assessment; dropping it changes nothing. Installation rechecks its deadline
and atomically acquires the logical cells. A framed job appends one dedicated
frame cell to at most seven ordinary cells. Generic requests/effects reject
active/closed journal, checkpoint and live-metadata quota kinds even at zero
amount; these require writer or maintenance transitions. The composed
coordinator supplies physically checked extensions for ordinary parts. A frame
reserves its full ceiling at initial admission and cannot be extended.

Only one dedicated append ticket may be in flight. Its actual byte/operation
amounts must fit the reserved ceilings; both ceilings and actual counts must
fit the minimum encoded size for that many operations. M08 still validates
the serialized frame. Proven durable completion moves exactly those amounts
from pending to used and atomically releases the unused frame remainder.
A frame reservation permits only one successful transaction; separated appends
cannot reuse its rounded physical budget. Proven no-write completion leaves
the reservation unchanged and permits retry. An uncertain
append keeps its ticket busy and pending and stops writer admission: the active
EOF may have a torn tail, so checkpoint rollover is unsafe before recovery.
Losing an append ticket also pins the writer. No generic effect can consume a
frame cell. An already consumed append ticket cannot stop a later retry on
the same frame; every result validates ticket liveness before changing phase.

A simulated checkpoint barrier refuses while an append is in flight and checks
closed-journal byte/segment caps before closing all new admission. It protects
the future logical transfer by excluding concurrent writer changes, without
posting a duplicate pending charge. Existing non-journal effects can finish
and outstanding jobs can resolve parts, enumerate expired leases and cancel
through the barrier. Read accessors remain available. Proven abort before any
selection reopens unchanged; a dropped barrier stops the writer. Uncertain
selection also stops the writer. Actual writer/view locking and pin eligibility remain M08.

After trusted durable selection, one quota transition adds the old active
journal's 96-byte header plus committed frame bytes to closed-journal usage,
adds one closed segment, and clears only committed active bytes/operations.
Outstanding frame reservations and their identifiers stay unchanged. The new
selected table length must fit the committed-only output bound and
live-metadata cap; uncommitted reservations cannot explain selected output.
Invalid selection leaves counters unchanged and stops the writer, because
the adapter reported an already durable on-disk change. Success enters
AwaitingSpace; there is deliberately no public reopen operation. M04c3b3 must
first establish a fresh probe and protect capacity for the next checkpoint.
Checkpoint building/retention quota and physical transfer are not implemented
by this scalar barrier. M08's exact live-metadata updates, selected identities,
publication proof and cleanup authority are also still required.

## 2. Free space and completion reserves

Keep 128 MiB of filesystem free space above all outstanding reservations, and
4096 free inodes where the filesystem reports a meaningful inode count.
M04 permits raising these floors, never lowering them below these defaults.
A worker samples available bytes/inodes before granting disk admission; a
cached health sample alone cannot authorize a new body or maintenance run.
Missing or failed byte probes refuse write startup/admission. Unsupported
inode reporting is explicit in health: enforce the file-count cap and ordinary
write-error behavior, without claiming an inode guarantee.
These floors apply independently to every backing filesystem used by the
service, including separate scratch/log mounts. A smaller dedicated mount is
an unsupported configuration and fails startup validation. Request/sort scratch
on tmpfs also counts against host/cgroup memory and is unsupported by the
default low-memory deployment profile; use disk-backed private scratch.

The physical reservation includes rounded remaining data growth, new files and
all namespaces sharing that filesystem. Round each growing file using the
probe's allocation unit, with checked arithmetic; metadata allocation overhead
is covered by the free-space floor rather than a false exact prediction.
Already written bytes are not outstanding bytes; moving a charge between those
states must be atomic in the coordinator. Filesystem compression, snapshots,
quotas and other host processes can still cause ENOSPC or sync errors. The
reservation does not prove that a future write will succeed.

Sampling must also account for writes concurrent with the probe. Per filesystem,
keep checked monotonic counters of completed charged byte growth/new inodes.
Capture them before starting the probe. Under the coordinator at grant time,
subtract every subsequent counter increment from the reported available space,
as well as remaining/pending-I/O reservations, completion reserve and floor.
An in-progress write remains reserved until completion atomically moves its
charge to those counters. A newer probe can reconcile older completed growth;
never credit deletion before a probe observes the freed space. This may count
some concurrent growth twice, conservatively, but cannot count it as free twice.
Use space available to an unprivileged user, not privileged reserved blocks.
Before granting, also check that completed plus pending, checkpoint and new
reserved growth fits the monotonic counter domain, including inode counts
when the physical inode probe is unsupported. Exhaustion refuses before I/O;
never wrap a counter after an admitted write has already happened.

The implemented space evaluator consumes one filesystem's successful probe,
its captured completed counters, current counters, protected checkpoint
capacity for the resulting state (including the new request) and a new
request rounded per file. The request retains its rounding unit; a probe
reporting a different unit refuses evaluation. It returns remaining bytes
and explicit available/unsupported inode headroom. It mutates no state and
grants no lease. The coordinator must match the open filesystem identity and
fresh probe, reject probe failure, and atomically install all passing
reservations and the resulting checkpoint reserve before another evaluation.
All namespaces on that filesystem share counters. The file-growth helper
assumes the old allocation was already charged; shrinking or deleting a file
gives no immediate physical credit. Round each file separately. A partial or
failed write retains its pending charge until the coordinator accounts for
its completed growth; unused reservation release cannot release written/orphan
bytes. Ordinary byte/inode shortage, including a floor larger than available
space, remains distinct from exhausted monotonic counters.

A free-space probe must use the actual open store/filesystem identity, not a
shell utility or a client-supplied path. Safe std currently supplies no portable
free-space/inode probe. M05 must implement a narrowly reviewed platform adapter
and amend UNSAFE.md plus the component contract before adding any unsafe call;
this design does not authorize an implementation or a new dependency. A fake
probe drives exact-boundary and concurrent-reservation tests first.

Before admitting each mutation, preserve enough additional physical capacity
to checkpoint its resulting state: the selected table lengths plus committed
journal bytes plus outstanding reserved frame bytes, plus 36 bytes for each
committed/reserved journal operation, and bounded new table/manifest/CURRENT
overhead within the additional 1 MiB generation allowance. A table record's
48-byte overhead exceeds a PUT operation header by 36 bytes; even DELETE/CHANGE
are conservatively counted in this correction. Directory inode reservations
and the filesystem metadata floor are separate. This is a conservative streaming
merge upper bound; replaced rows and tombstones can only reduce the output.
Keep this reserve distinct from admitted body/WAL/response completion charges.
It is reusable checkpoint capacity, not a new full reservation per client.
The checkpoint helper returns an unrounded total. When individual output
lengths are not yet known, bound the sum of per-file rounded lengths by
`round_up(total, unit) + (file_count - 1) * unit`, with checked arithmetic.
Count every prospective file, including all eleven tables, manifest and
CURRENT temporary; reserve new directory/file inodes separately. This padding
is required even when an allocation unit exceeds the 1 MiB format allowance.
The RoundedGrowth total-bound constructor implements this conservative bound.
In addition to the thirteen generation files and fourteen file/directory
inodes, protect a fresh active journal: round its 96-byte header separately
and reserve one more inode. Closing the old journal changes its logical quota
category, without physical growth from the rename itself. Transfer outstanding
leases to the new journal during the checkpoint barrier; they keep their
charges and identities and acquire no sequence early. Transferable WAL leases
reserve `round_up(frame_bytes, unit)` independently of the old active EOF;
its available trailing block cannot be assumed to exist at the new journal's
EOF. The bound `round_up(x + n) - round_up(x) <= round_up(n)` preserves capacity
across that rebase. M04c3b3 enforces these physical reservations and transfers.

Maintenance, cache rebuilds, logging and new requests cannot borrow the space
needed to finish admitted commits or the last permitted checkpoint. If retiring
the current generation would exceed two pinned retired generations, defer that
checkpoint/admission; an unpinned current generation adds no retired pin.
Never remove a pin to force progress. Checkpoint/scratch quota
pressure is retryable storage unavailability. It is not permission to delete
acknowledged mail, failed submissions or advertised/pinned history.

An online backup writes to an explicitly chosen external destination, with
independent free-space checks and a bounded output reservation. Its output is
outside store quotas; source generation/body pins still count normally. Do not
create a backup hardlink farm or silently use the active store as its target.

### Filesystem registration and probe matching

M04c3b3a keeps at most sixteen caller-backed filesystem entries. All configured
namespaces sharing available-space capacity must receive the same BackingKey
from the trusted adapter and use the same registered FilesystemId. Duplicate
keys reuse a slot; an inconsistent allocation unit refuses. The key is an
adapter-assigned value, not a pathname or a promise that a kernel device number
alone identifies shared capacity. M05 must establish it from pinned descriptors
and account for mount aliases. No actual identity discovery happens here.
Registration uses checked process-wide slot generations, so a retired,
reconstructed or foreign registry's identifier cannot authorize a new entry.

The registry captures monotonic completed growth before a worker begins the
probe, retaining its start Tick as well as its deadline. A non-Clone ProbeTicket
becomes a non-Clone Observation only after a
successful adapter result with a nonzero allocation unit. Failed probes produce
no observation. Multiple probes can overlap and finish out of order; no
latest-only serial rejects an otherwise valid earlier sample. The consumer
removes an observation from its caller-owned Option on every attempt, including
refusal. It matches the live filesystem identifier, deadline and pinned unit,
and refuses regressed completed byte or inode counters. Deadline equality is
expired, and a current Tick before the captured start is invalid. Unsupported
inode reporting remains explicit in the sample. The composed coordinator must
clamp the probe window to `WorkLimits::admission_seconds` and the enclosing
deadline;
an arbitrary request deadline is not a probe freshness bound.

A CheckedSample retains identity, start/deadline, captured counters and sample. It
is matched data, never a lease or I/O permission. The composed atomic grant
must consume it by value and recheck its identity, deadline, allocation unit
and counter ordering under the same exclusive coordinator state that installs
the reservation. Checkpoint reopening requires a probe begun after selection,
with an ordering fence owned by the barrier; equal millisecond Tick values alone
cannot establish that order. M04c3b3c must implement that fence rather than
reuse a pre-selection sample with an unexpired request deadline.
Precompute every filesystem counter/reference update before
installing lease cells; publish those updates infallibly only after successful
logical installation. Failed multi-filesystem grants discard all their samples.
The registry itself exposes no public counter mutation; the composed
coordinator owns its accounting transitions.

Filesystem retirement refuses pending growth, protected checkpoint capacity
or any live lease reference, including a lease with zero physical growth.
Outstanding probes own no capacity; retiring/re-registering makes their old
identifiers stale. Completed counters may reset only with that new identity
and a fresh probe. M05's owner detaches every configured path alias before
retiring their shared entry; registration does not count path attachments.
Dropping the registry leaves occupied cells and prevents accidental table reuse.
An orderly reset reconciles effects through the live coordinator before drop.
An abandoned coordinator requires full store reconciliation under LOCK before
fresh bookkeeping is constructed; inspecting or blindly clearing old cells is
not reconciliation. The guard is not an integrity boundary against the trusted
backing-memory owner.
Unexpected post-acquisition bookkeeping failures poison the registry.

### Combined reservation implementation

M04c3b3b's Coordinator owns the writer ledger and filesystem registry with no
mutable escape to either. Its metadata filesystem identities are fixed at
construction; the entire registered filesystem set is fixed in this increment,
including log/scratch mounts. Registration/retirement remain standalone
registry operations until configuration integration supplies an owned route.
It accepts only a pristine logical lease table; recovered usage
and selected lengths still come from one trusted startup snapshot. It starts
closed, then initializes only after fresh samples cover the separately rounded
generation output and fresh journal header. Metadata locations sharing one
filesystem aggregate their demand before applying that filesystem's floor.

Each ordinary request supplies four logical charges and one rounded physical
budget bound to a registered filesystem. One group has at most eight parts,
including its optional last, dedicated journal part. The physical binding is
stored in the same lease cell. The adapter owns the proof that these budgets
cover the proposed objects and categories. This API does not infer file
identity, make syscalls or prove serialized journal contents.

Admission consumes all supplied CheckedSamples, including on refusal. It
rechecks identity, allocation unit, start/deadline, maximum probe age and
completed counter ordering while holding both ledgers exclusively. Every
requested filesystem and both metadata locations require exactly one sample;
duplicate, unnecessary or missing samples refuse. Slot-generation contention
also discards all samples without publishing charges; the scheduler retries
with new probes on a later turn. The assessment uses old
pending growth, the resulting checkpoint reserve and new growth once each.
A fixed filesystem projection validates byte/inode additions and lease
references before logical installation. Logical installation precomputes all
records and returned tokens before publication. The final filesystem publish
has no fallible operation. No successful logical grant can escape without its
physical reservation, and ordinary failures publish neither ledger.

Ordinary extensions use fresh samples and the same staging rule without
allocating a new cell or reference. Frame reservations cover one complete
transaction and use a rounded frame-length ceiling independent of active EOF.
A proven synchronized append charges actual rounded growth and releases all
unused logical/physical frame capacity together. Proven no-write permits retry;
uncertain append keeps its pending capacity and busy ticket and stops the
writer.
The dedicated frame cannot be extended or reused after a successful append.

Ordinary I/O uses linear tickets bounding both logical and physical effects.
Proven completion charges only reported bounded effects; uncertain completion
charges the whole plan. Invalid proof leaves both ledgers and the live ticket
unchanged. Existing I/O can finish after expiry or a writer stop. Cancellation
refuses busy groups and releases unused growth plus one filesystem reference
per cell. Used/orphan charges and monotonic completed growth remain. Fully
spent or zero-growth cells retain their filesystem reference until cancellation.
Cancellation and short journal completion may leave checkpoint protection
conservatively high; the next fresh admission recomputes it. No deletion credit
or used-quota cleanup is inferred from freeing a lease.

Checkpoint building reservations, quota overlap, selection and reopening are
not exposed by this coordinator yet. M04c3b3c must transfer protected capacity
and enforce post-selection probe ordering before adding those operations.

## 3. Work budgets and deadlines

Use Clock's monotonic Tick for deadlines; persisted UTC is not a timeout clock.
Every job carries an absolute deadline and checked remaining counters for bytes
read/written, records examined and output bytes. Charge repeated reads and
merge passes again. Stop on the first exhausted dimension. A counter never
wraps, and partial scans are never returned as complete query/search results.

The implemented Meter charges I/O bytes, records, output bytes and GC
unlinks atomically before a bounded step. A rejected charge consumes no
counters and permanently stops that meter at its first refusal, including
deadline expiry. Charge failed/repeated operations too. Construct a meter
with validated job limits and the minimum of all enclosing deadlines. It
neither refunds durable effects nor cancels I/O; callers keep completion
reservations for work whose effects already started. This helper does not
implement scheduler step limits.

A CPU/I/O scheduling step handles at most 64 KiB or 256 records before yielding
and checking cancellation/deadlines. One issued filesystem operation can block
past the deadline; do not promise kernel-I/O cancellation or spawn a replacement
worker. RESOURCES.md's fixed workers, queues, stricter per-role step limits and
shared sort lease still apply.

| Job | Default total budget |
| --- | --- |
| Foreground mail get/query/search | 120 seconds, 8 GiB scanned bytes, 2000000 examined records |
| /changes | 30 seconds, 128 MiB history bytes, 1000000 operations |
| One JMAP request's method execution | 300 seconds, including waits and all method work |
| Writer commit after complete input admission | 30 seconds, one frame, 256 MiB validation reads and 250000 examined records |
| Checkpoint | 60 seconds, 2 GiB combined read/write bytes |
| GC drain | 30 seconds; do not hold writer lock while draining |
| GC exclusive window | 120 seconds, 8 GiB combined I/O, 128000000 records, at most 1000 unlinks |
| Online backup | 900 seconds, 16 GiB combined I/O, one existing read view |
| Admission/pool wait | 1 second, within the enclosing job deadline |

M04 accepts finite raised values up to 16 times each default, using checked
multiplication; lowering a work limit below the default is not a v1 option.
The fixed frame/journal/count-format ceilings cannot be raised through these
settings. A larger corpus or slow disk may require a raised work budget even
when its disk quota fits. Operators see the exhausted dimension and job kind
in bounded logs/health. This is an explicit failure, not an incomplete success.

Validate maintenance work against configured capacities before startup:

- Checkpoint I/O >= `2 * (live_metadata_cap + journal_bytes + 1 MiB)`.
- GC I/O >= `4 * live_metadata_cap + 16 * sort_disk_bytes +
  8 * journal_bytes + 16 MiB`.
- GC examined records >= `16 * (live_metadata_cap / 64 + body_file_cap) +
  16 * journal_operations`. A checkpoint row is at least 64 bytes.
- GC exclusive time >= checkpoint time + one commit time. The GC's initial
  checkpoint consumes the same exclusive time/I/O/record budget.

These conservative bounds cover checkpointing and at least one complete
candidate-window proof at configured metadata/file caps, including a shard
scan and current owning-reference scans. GC does not require a full-mailbox
external sort before it can make progress. If real I/O cannot finish even one
proof within the wall-clock window, defer safely and report
maintenance_budget_insufficient; do not report successful reclamation or keep
retrying a known-insufficient window without actionable health failure. M08/M23
must demonstrate repeated-window progress on the configured maximum corpus.

The admission wait covers acquisition of a new pool/reservation, not each step
of already admitted work. Maintenance deliberately refuses new work promptly
with temporary errors; callers need not wait through its entire pause. GC
drains admitted work before taking exclusivity, so it cannot strand an active
commit behind an exclusive sweep. A checkpoint may defer queued mutations
within their enclosing deadlines. This is the explicit v1 availability
tradeoff, not a promise that every request waits for maintenance to finish.

The GC exclusive phase uses 128 fixed 32-byte candidate cells (4 KiB) from
the writer's existing 256 KiB scratch. Each records ID, namespace and live bit.
Visit message/upload shards in namespace, shard, then raw-ID order. A bounded
selection pass keeps the next 128 distinct IDs after the cursor from inventory
and validated directory entries; it does not depend on directory iteration
order. Stream all current owning references to mark this window before unlink.
Physical entry enumeration consumes record/work budget too.

Commit/unlink batches contain at most 100 proven-dead candidates and no more
than one frame. Do not begin a batch without completion capacity. On expiry
finish an admitted durable batch, then resume service; discard incomplete
liveness proofs. Advance the cursor only through candidates whose keep/delete
decision finished, including completed unlink/sync for dead files; live entries
advance it too. A partly processed window resumes after the last such entry.
Each new exclusive window must obtain a fresh view
and repeat its reference proof; cross-generation cursor progress never grants
permission to unlink. Deleted entries do not invalidate the ordering cursor.

Persist the scheduling hint as at most 256 bytes at cache/gc-cursor, using an
exclusive temporary file and atomic replacement. Reserve 1 KiB and two file
entries within the cache quota for this purpose. Its ASCII record is exactly
`gc1 EPOCH NAMESPACE SHARD LAST\n`: EPOCH is 32 lowercase hex, NAMESPACE is
messages or uploads, SHARD is two lowercase hex, LAST is a 32-lowercase-hex ID
or '-' for the beginning of a shard. Require LAST's prefix to match SHARD;
reject extra fields/bytes. A missing, malformed or foreign-epoch cursor starts
at messages/00/-; after uploads/ff, wrap there. It is disposable, needs no
fsync and carries no liveness evidence. Failed cursor persistence is visible
health/work failure, not permission to reuse a stale proof.

Sort merge fan-in is eight, with at most 32 open files per sort job including
inputs/output/bookkeeping. Its existing RAM arena and disk quota include runs
coexisting with their replacements. These sorts serve queries/indexes; GC's
candidate proof does not depend on enough scratch to hold every live reference.

A checkpoint timeout before selection discards only proven-unselected scratch;
it cannot publish a partial table or advance CURRENT. After a publication
attempt whose durability is uncertain, stop the writer for recovery. A read
view's deadline is the enclosing operation's deadline. A completed JMAP response
spool releases its store view before waiting for a slow HTTP reader. Queue body
views and backup views use their own finite budgets below/above, not an
unbounded exception to retirement limits.
All long background views (outbound transfer, backup, cache build) share one
background permit; never start a backup and queue body view concurrently in
the default two-view profile. Reserve at least one view for foreground work.
An online configuration enabling these background operations requires at least
two views; the one-view profile is for foreground/offline operation. Waiting
for the background permit occurs before starting an outbound attempt. Rotate
pending background job classes when the permit becomes available.

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
not journal records or backups, and do not need fsync. Charge every byte before
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
undo journal state or fabricate failure of an already committed object. A
spool fault alone is not journal corruption; report the actual failure domain.
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

M04c1 tests pin numeric default/derived caps, exact response and checkpoint
quota boundaries, larger-metadata maintenance requirements, one-view refusal
for online background work, logical subquotas below the per-object ceiling,
invalid ranges/fixed overhead floors and helper-level arithmetic overflow.
Current configuration ranges prevent plan-level overflow. Disk quota changes leave the
RAM plan unchanged. These tests do not establish observed-use reconciliation,
safe quota reduction, live reservations, filesystem availability or runtime
work charging; those remain the implementation gates below.

M04c2 tests pin exact counter exhaustion, atomic/sticky refusal, absolute
expiry, HTTP ceiling division and slow-rate budgets, outer exchange sums,
SMTP recipient/block/fence budgets, independently raised phases/work limits
and Tick overflow. They
do not exercise sockets, scheduler priority, cancellation or allocation/RSS.
Those remain tests at the consumers below.

M04c3a fake-sample tests pin per-file rounding, exact byte/inode floors,
pending/last-checkpoint preservation, completion transitions concurrent with
a probe, fresh-probe reconciliation, unsupported inodes, counter regression,
pre-effect counter headroom (including pending/checkpoint growth), allocation
unit matching, multi-file rounding slack and the journal-operation correction. These are
arithmetic tests, not evidence of fresh probe matching, reservation ownership,
atomic live grants, orphan recovery or an actual filesystem adapter.

M04c3b1 tests pin independently configured quota mappings, duplicate grouped
charges, all-or-nothing cap/late-ticket refusal, fixed capacity and record
layout, stale/foreign tokens, extensions, effect pinning, uncertain/orphan
charges surviving cancellation, deadline/completion separation and rejection
of startup use above a cap. Expiry enumeration covers short output buffers and
busy non-root members; overflow and disabled extensions preserve the book.
These tests exercise logical state transitions;
physical coupling, writer barriers, actual cleanup and live probes remain
M04c3b3/M05/M08 gates.

M04c3b2 tests derive checkpoint bounds from used/pending/candidate counters,
exercise serialized proven/uncertain appends, reject generic writer-quota
bypasses, check barrier abort/refusal and closed-journal caps, and preserve
outstanding IDs/charges across scalar rollover. These are conditional
accounting tests, not filesystem publication or view-pin evidence. Additional
cases pin append-ticket replay, one successful transaction per frame lease,
barrier expiry handling, stopped completion handling, dropped/uncertain
barriers, committed-only selection bounds and exact closed-quota fits.

M04c3b3a tests cover registry/constructor bounds, occupied state, reconstruction,
poisoned refusal, alias deduplication, stale/foreign identity, single observation
consumption, overlapping probes, start/deadline metadata and both counter
regressions. Counter changes are injected fixture state to exercise retirement
and probe matching. These tests perform no filesystem syscall, real identity
derivation, live growth accounting or atomic physical/logical grant.

M04c3b3b tests exercise coupled grants on shared and distinct filesystems,
late shortage with no publication, completed growth during a probe, consumed
expired/over-age and unnecessary samples, partial/uncertain effects,
invalid and foreign tickets,
quota failure after physical extension staging, one-shot frame completion,
writer-stop accounting and all 64 cells including physical bindings. These use
injected probes and effect proofs; real filesystem effects remain M05/M08.

M04/M05/M08/M13 add tests at their real execution boundaries, not document-only
assertions: concurrent quota reservations cannot overbook; failed publications
keep orphan charges; byte/inode probes fail closed as specified; admitted
commit space survives a cache/GC attempt; pinned generations block checkpoint
admission; every maintenance limit resumes service without unsafe reclamation.
Inject failures before/after selection, sync and response-file writes.
Include minimum-row metadata at configured caps, at least one full successful
GC traversal across bounded proof/unlink windows, startup private-body/unselected-checkpoint cleanup,
concurrent-probe counter transitions, and history pins across journal rollover.

M09/M11/M17 own DNS, receiving and smart-host timer/slow-peer tests; M13 owns
HTTP execution-versus-transmission timers and per-byte transfer bounds. M20
tests online backup/view arbitration and destination capacity; M21 tests finite
offline page/blob transfer at the admitted minimum rate. M23 measures background
fairness and maximum-corpus progress without increasing the RAM ledger.

JMAP tests chain query, mutation and reference and prove the reference sees the
original query result. Cover duplicate call IDs/implicit responses, nested
wildcards/escaping, creation maps, input-depth versus MIME output-depth, exact
spool exhaustion before and after earlier method/object commits, unlimited body
values, slow HTTP readers releasing store views, and cleanup after disconnect
or process death. Allocation counters cover spool walkers and repeated lookups.

Sources: [JMAP Core, RFC 8620](https://www.rfc-editor.org/rfc/rfc8620.html)
and [SMTP timers, RFC 5321 section 4.5.3.2](https://www.rfc-editor.org/rfc/rfc5321.html#section-4.5.3.2).
