# Startup resource ledger

M01 and M02c3a implement `Limits::plan` in [src/limits.rs](src/limits.rs).
It validates
counts, individual ceilings, relationships and checked arithmetic before
returning a fixed-size ledger. It allocates no pools and starts no workers.
`ConfigVersion` versions operator configuration, not the unimplemented disk
format. Local IDs are distinct 16-byte types with canonical lowercase hex;
they convey no authorization. Derived MIME-part wire IDs belong to M02.

## Default reservation

All quantities below are bytes. This is a planned ceiling for each component,
not a claim that the service exists or its RSS has been measured. Ownership below assigns
concrete structures/workers within it; M04 implements pools; M07 measures TLS;
M23 verifies resident usage. A structure exceeding its reservation must change
the checked ledger and pass the budget gate before admission is enabled.

| Component | Count | Bytes each | Total |
| --- | ---: | ---: | ---: |
| SMTP slots | 8 | 376064 | 3008512 |
| HTTPS slots | 8 | 1703936 | 13631488 |
| Body/search jobs | 2 | 425984 | 851968 |
| Storage read views | 2 | 4755456 | 9510912 |
| Writer and checkpoint | 1 | 6946816 | 6946816 |
| Resident index cache | 1 | 8388608 | 8388608 |
| Outbound slots | 1 | 572672 | 572672 |
| Sort runs and merge buffers | 1 | 1048576 | 1048576 |
| Log queue and formatting | 1 | 131072 | 131072 |
| DNS, ACME and control scratch | 1 | 524288 | 524288 |
| Slot queues and queue window | 1 | 131072 | 131072 |
| Fixed worker stacks | 8 | 262144 | 2097152 |
| Main stack allowance | 1 | 1048576 | 1048576 |
| TLS session headroom | 17 | 524288 | 8912896 |
| TLS handshake headroom | 2 | 4194304 | 8388608 |
| Established TLS processing | 1 | 4194304 | 4194304 |
| Certificate generations | 2 | 8388608 | 16777216 |
| Cold reload overlap | 1 | 2097152 | 2097152 |
| Process and allocator allowance | 1 | 8388608 | 8388608 |
| **Total** | | | **96650496** |

The total is approximately 92.17 MiB against a 96 MiB configured budget;
the remaining 4012800 bytes are unassigned headroom, not another cache.
With all other defaults, three TLS handshake slots require 100844800 bytes;
the default 96 MiB budget refuses that profile. Increase the configured memory
budget explicitly when adding that third handshake slot.
The 64 MiB idle and 128 MiB workload RSS release ceilings remain independent
and unverified. The TLS entries are demand headroom, not additional arenas to
allocate and touch at startup. Idle retains its actual current generation;
provider state for unused session/handshake slots, processing headroom and
the replacement generation are not expected to be resident. Application arenas
and owned TLS wire pools still must be allocated and touched before admission;
those wire pools are already charged within the session entries. The ledger sum cannot establish either
RSS result; nondefault pool sizes require separate qualification.

The TLS entries are qualification ceilings. They give the measured fixtures
room for retained peer chains, native caches, decoder expansion and complete
generation overlap, but are not proven maxima for every admitted input.
The current Rust/native counters constrain requested bytes in overlapping
domains; they do not measure allocator overhead or every realloc transient.
Increasing these entries does not qualify service admission or RSS. M07e must
still account for those costs and concurrent owners within the ledger.

The generated Unicode arrays have a checked 58720-byte compiled payload.
Named static slices give each table one runtime allocation identity.
M06o exposes them through fixed library lookups. Their runtime static data
belongs to the existing process/allocator allowance. Slice metadata,
accessor code and mapped-page rounding are excluded from this payload count
and also belong within that allowance. No new arena or RSS qualification
follows from this payload count.

The generated positive leap-date array has a checked 108-byte payload in
the same process/allocator allowance. Cold input/generator files are not
linked into the service. Date qualification uses scalar locals and at most
five checked table comparisons, prepaid as six extra records. No runtime
source read, allocation or new arena is introduced. Source metadata remains
in the checked-in provenance; mapped pages and code are outside the array
payload count and still belong within the process allowance.

Resident NFC uses the existing 4 KiB conversion reservation: 3072 bytes of
caller-owned scratch plus at most 1024 bytes for cursor state and its shared
header budget. Four private source checkpoints are included in that cursor
bound. Tests pin these layouts and exercise fast/replay turns without Rust
allocation. This does not measure compiler stack frames, allocator overhead,
encoded-header adapters or combined service RSS.

## Slot composition and ownership

- SMTP: bounded headers, 320 bytes per envelope recipient, 64 KiB I/O and
  16 KiB state. The fixed recipient cell must hold the SMTP path plus metadata.
- HTTPS: request bytes, 16 bytes per JSON token, 128 bytes per largest
  get/set/query result window entry, and 96 KiB framing/output scratch.
  Escaped values are streamed; property keys are unescaped in place before
  request tokens borrow the request arena. Unescaping cannot expand the UTF-8
  byte length; the future JSON parser must validate escapes and update token
  extents before immutable borrowing. No separate decoded-key arena is owned.
  Earlier method results and created-ID maps use the bounded disk retention
  contract in ADMISSION.md, never extra per-method heap trees. Event streams use slots
  without pinning storage views between emissions.
- Body job: headers, 64 bytes per MIME descriptor, 96 KiB decode/work scratch.
  Nested parsing and transfer decoding share that reservation.
- Read view: 4 MiB journal prefix, 32 bytes per journal operation, 292 KiB
  cursor/value scratch. Backup consumes an existing view.
  The replay Overlay borrows the immutable frame arena and at most 8192
  caller-owned Cell values. Limits::plan checks each compiled Cell fits the
  32-byte journal slot. Cells retain offsets, lengths, sequence, ordinal,
  tag and operation kind; in-place unstable sorting adds no heap inventory.
  Construction bounds work by the entire arena/count; point/next lookup uses
  binary search and a fixed 1024-byte key scratch for typed get. The dedicated
  allocation probe builds and sorts 8192 operations with repeated/permuted keys,
  performs successful/absent point and next lookups, and refuses a short slot
  buffer and corrupt frame without Rust allocations after cold preparation.
  The provisional table Merge borrows the same overlay, retaining a pending
  borrowed entry, scalar progress and a fixed 1024-byte previous checkpoint key.
  Each push/finish initializes one 1024-byte overlay-key stack buffer and reuses
  it while emitting borrowed rows. Merge's previous key is inline state, not a
  borrow from the per-view cursor/value arena. Tests cap the compiled Merge at
  4 KiB; this state and its temporary key buffer are charged to the owning worker
  stack. Runtime integration still must qualify the complete worker stack.
  TableLookup borrows a target key and a distinct result buffer while retaining
  the replay's record scratch. The 66608-byte maximum table record has its own
  68 KiB partition; the 64 KiB result, 1 KiB key and existing 63 KiB cursor
  allowances are separate at simultaneous peak. The default two views add
  136 KiB to the plan, within the unchanged 96 MiB budget. Tests pin this
  partition sum and the maximum record extent. The allocation probe exercises
  present/absent/deleted lookups, completion and insufficient-result refusal
  at both root bounds, with all buffers prepared before measurement.
  TableNext uses those same record/value partitions and the 1 KiB output-key
  partition. Its borrowed input cursor is at most 1 KiB within the existing
  cursor allowance; comparison uses another fixed 1 KiB worker-stack buffer
  until a candidate is captured. Tests cap compiled TableNext at 8 KiB; this
  inline state plus its temporary buffer stays charged to the worker stack,
  whose full runtime qualification remains pending. The allocation probe covers
  initial/equal/later cursors, deletions, exhaustion and short-key refusal at
  both root bounds. No additional pool or whole-table map is allocated. Admit one input
  record plus up to 8192 overlay keys per push, and up to 8192 keys for finish.
  The allocation executable checks unchanged/replaced/deleted rows, remaining
  output, input/sink failures, and draining the full-operation overlay after
  cold preparation. Callback allocations remain the caller's responsibility.
  ParentWalk copies three mailbox IDs, the view identity and scalar progress;
  tests cap compiled state at 256 bytes, charged to its owning worker stack.
  Each advance borrows the existing result buffer for one ReadView get and
  releases row strings before the next lookup. The caller's explicit lookup
  budget bounds total operations; each get retains the view's own work limits.
  No visited-ID collection, pool or additional result arena is created. The
  allocation probe covers a rooted chain, a cycle, a missing target and read
  exhaustion using a fixed view and caller buffer after cold preparation.
  ReferenceCheck retains at most two typed direct targets, borrowed source key,
  identity/time and scalar progress. Its compiled state fits 512 stack bytes;
  the source key uses the caller's existing key storage while each get borrows
  the separate result buffer. A key borrowed from next's Record also retains
  the shared value-buffer lifetime: the driver first detaches it into existing
  independent cursor/key scratch or charged stack storage, then decodes from
  that borrow before reuse.
  That scratch remains separately charged; ReferenceCheck allocates none.
  Total lookup count is at most two. No collection
  or arena is added. Allocation instrumentation checks all source table kinds,
  live/expired leases, historical-ID omission, zero-target completion and sticky
  missing-target refusal using a fixed view and stack scratch.
  Reference Sweep stores one fixed 1 KiB prior key, 11 counters and scalar
  progress; compiled state fits 2 KiB. Each advance uses a separate 1 KiB stack
  key copy to release next's shared key/value lifetime before reference gets.
  The inline state, temporary key and ReferenceCheck fit the owning worker's
  existing stack allowance; whole-worker qualification remains pending. Caller
  key/value output partitions are reused without a new arena. A finite row
  limit bounds successful enumeration; each step admits one next plus at most
  two gets. Allocation probes cover populated and empty all-table sweeps.
  Recipient Sweep retains typed previous IDs/ordinal, one current group and
  scalar counters/phase. Compiled state fits 512 bytes; each advance encodes at
  most a 20-byte cursor on the worker stack and performs one next using existing
  key/value result partitions. A finite combined row allowance covers both
  streams. Allocation instrumentation covers exact multiple-group and empty
  coverage with fixed input slices; no recipient collection is constructed.
  Queue consistency adds only scalar group flags and borrowed reply-code checks,
  preserving the 512-byte state bound and one-next-per-advance contract. No
  separate queue scan, string copy or allocation is added.
  Mailbox Sweep retains one ParentWalk, previous mailbox ID and scalar
  progress; compiled state fits 512 bytes on the worker stack. It reuses caller
  key/value outputs and adds no arena or visited set. Separate total row/get
  allowances cover enumeration and repeated chain traversal. Each walk also
  re-reads its starting row: total gets sum chain lengths including start and
  root, so N root mailboxes need N gets. Shared chains may require quadratic
  gets; the admission budget must account for that cost
  and each lookup's full physical work. Allocation probes cover rooted shared
  chains and an empty forest using fixed slices and result scratch.
  BlobSweep retains one private BlobInput/digest, a copied pending BlobRow,
  prior ID and scalar progress. With the shipped Provider its compiled state
  fits 1 KiB on the worker stack; there is no new arena. Enumeration value
  scratch becomes the at-most-64-KiB chunk buffer after row detachment. Finite
  row and cumulative-byte allowances precede opens; only one file is retained.
  Each step admits one next/open/read/completion operation and its deadline.
  Allocation probes cover empty/populated views and row/byte refusal over
  prepared files at both supported root lengths.
  TableSweep retains one TableReplay, selected metadata/overlay borrows and
  scalar/per-table counts. With the shipped Provider its compiled state fits
  4 KiB on the worker stack. Completion also moves one TableReplay out of
  the retained option and uses Merge's temporary 1 KiB key; those temporaries
  are charged to the same worker stack, whose full qualification remains pending.
  The same charged record partition transfers between
  tables only after completion; no per-table arena is allocated. Descriptor
  byte totals are admitted before table I/O and a finite row allowance covers
  final merge output. Per-step overlay work retains Merge's bounded cost.
  Allocation probes cover all-table replay and scratch reuse at both roots.
  StoppedStore moves the existing LockedRoot (including its directory and lock
  descriptors) without another allocation, descriptor or synchronization object.
  Its operations reuse the existing metadata, overlay, record and change arenas.
  FileValidation holds both sweep states (only one performs I/O at a time),
  completion summaries and buffer references; its Provider state fits 8 KiB.
  Constructing both sweeps and the returned coordinator, or moving a completed
  sweep, can create additional stack temporaries; whole-worker stack
  qualification remains pending. It transfers the existing record buffer from
  table to history work and returns the existing change slots, adding no arena.
  ReservedAppend adds an exclusive borrow of the existing WriterLedger and one
  existing append ticket; the combined state fits 2 KiB. ReconciledAppend retains
  only the durable evidence and fits 1 KiB. The guard allocates no
  ledger, arena or reservation cells. Caller-owned fixed quota/slot backing is
  reused, with full charges retained on abandonment until recovery. Production
  stack and maximum-data allocation qualification remain pending for this path.
  The dedicated allocation process measures reservation creation and physical
  append through terminal reconciliation/disposal in sixteen intervals: short
  and maximum roots, each with ordinary completion, successful short writes,
  partial write failure, sync failure, final-length refusal, and abandonment
  before writing, after writing and after confirmation. Every allocator counter
  must remain identical, including frees, failures and live/peak bytes. Ordinary
  completion and abandonment use the public advance and production Real I/O;
  short-write and failure scenarios use the injected operations. Root,
  selection, scan, encoded frame and ledger backing are prepared before each
  interval; fixture cleanup and ledger destruction follow it. This uses small
  frames and injected failure boundaries, not maximum-size datasets, native
  allocation, worker-stack or whole-service RSS qualification.
  JournalAppend and SyncedAppend each fit 1 KiB of Provider state. An append
  borrows the existing immutable transaction-frame buffer until finish/drop
  and keeps one writable descriptor after comparing it to the consumed prior
  boundary's descriptor. Every advance writes at most 64 KiB, syncs once, or
  checks final metadata/EOF; 64 total write calls cap pathological short-write
  progress. It adds no arena, queue or thread. Construction rechecks CURRENT
  using two fixed 120-byte buffers, up to 64 reads plus EOF and one transient
  descriptor. After that closes, the prior boundary descriptor overlaps the
  new writable descriptor: a read-only scan for the first append, or the
  retained previous append thereafter. Opening also uses existing fixed path
  scratch and transient parent handles. Each successor consumes the prior
  evidence and returns its next owner. The caller may refill the same frame
  buffer after finish and reuse its ledger; no additional frame arena or full
  journal reread is needed between successes. Constructor frame parsing/hash
  and std open work still need caller admission, and whole-worker
  stack/allocation/resource qualification remains pending for this new append
  path.

  JournalCapture and CapturedJournal each fit 2 KiB of provider state. Capture
  borrows the existing 1 MiB whole-frame recovery scratch; its consuming finish
  releases that borrow for reuse, with no new arena. Overlay loading still uses
  the existing 4 MiB replay arena: the 1 MiB scratch suffices only for prefixes
  whose frame bytes fit. Comparing the two prefix digests adds no I/O. Each
  advance/finish still requires caller admission and deadline checks. The
  retained scan descriptor
  overlaps the reopened prefix descriptor during overlay loading; release the
  capture after handoff. No descriptor registry or serving lease is implied.
  Tests allocate their frame storage before scanning; full-worker stack and
  whole-process resource qualification remain separate.
  DataValidation retains one ValidationView, the four fixed sweep states and
  their scalar completion evidence. Provider state fits 8 KiB on the existing
  worker stack; only one sweep does I/O at a time. Construction/moves and inner
  lookup/replay/reference temporaries remain additional stack work awaiting
  whole-worker qualification. No arena is added. Caller key/value storage is
  reused for every phase, with value storage becoming blob chunk scratch;
  record/change scratch borrows end on finish/drop independently of the retained
  CheckedData file/owner proof. Finite row, parent-get and blob-byte limits plus
  the reader work/deadline bounds apply. Existing component allocation probes
  remain; this composition adds no whole-service allocation or RSS claim.
  ValidationView retains the checked snapshot borrow, one ChangeScan, a clock
  guard and scalar limits. Its Provider state fits 4 KiB, excluding the temporary
  TableLookup/TableNext constructed during a row call and their existing merge
  temporaries. Whole-worker stack qualification remains pending. Existing record
  scratch is shared across row reads and change steps; retained change slots
  remain separate. No arena, per-operation collection or thread is added.
  The blocking verify_account composition borrows these existing partitions
  together: selection, 1 MiB recovery frame, 4 MiB replay bytes, overlay cells,
  record/change slots and key/value buffers. It allocates no additional arena.
  The frame scratch is idle after capture; physical validation transfers its
  record/change buffers into data validation. The capture descriptor is dropped
  after overlay loading, and all input descriptors close before return. Its
  VerifiedAccount summary fits 2 KiB and borrows only the stopped owner. Local
  coordinator construction/moves and nested reader temporaries still require
  whole-worker stack qualification. The dedicated Rust allocator probe now
  measures this composition on small empty/blob fixtures at short and maximum
  roots, including refusals and persisted corruption. Fixture construction and
  buffers are cold; no whole-service RSS or native allocation claim follows.
  One fixed atomic watermark lets outer and nested clock samples share
  regression/deadline checks without an allocated registry.

  VerifiedStore fits 2 KiB including the consumed stopped root and its selected
  CURRENT/identity/journal summary. Its verification borrows the same cold
  VerifyScratch partitions; no read descriptor or scratch borrow survives.
  This replaces the stopped owner at completion and adds no pool or arena.
  Its whole-worker stack and allocation qualification remain pending.

  JournalSession fits 8 KiB, including the existing writer ledger, one
  retained journal boundary and two mutex states. It borrows the consumed
  VerifiedStore for its whole scope. Caller ledger cells/slots remain
  preallocated; startup reuses selection and recovery-frame scratch,
  returning the frame to the scoped callback. Reader pins use a bounded
  counter (configured one through eight) and a borrowed owner plus copied
  identity, without a new pool or arena. No filesystem I/O occurs under the
  publication mutex. The Rust allocation probe measures public commit and
  reader capture/drop in sixteen intervals: short and maximum roots crossed
  with repeated public commits, an initial deadline followed by success,
  deadlines after write/sync/reconciliation/publication, malformed frame
  refusal and capacity/lock contention. Reservations, physical append,
  reconciliation and visibility run inside measurement; fixture creation,
  owned verification, ledger/session startup, frame construction and owner
  teardown remain cold. It uses small 160-byte frames and requires all
  allocator counters unchanged, including frees. This does not qualify
  maximum datasets, concurrent worker stacks, native allocation, session
  startup or whole-service RSS. Runtime read scratch-pool leasing is
  described below; query scopes are qualified separately below.

  Pinned query preparation borrows one captured pin exclusively and reuses
  caller selection, replay frame/cell, record and change partitions. Its
  5032-byte SelectionScratch uses 5 KiB of the existing 292 KiB cursor/value
  reservation. Pool bookkeeping takes 1 KiB and pinned-body state takes 4
  KiB, leaving 53 KiB for cursor/history/index/checksum state. It keeps one
  LoadedOverlay descriptor while the existing table/history sweeps reuse
  record/change scratch, then lends ValidationView (the existing 4 KiB state
  bound) only within a callback. No second recovery-frame arena or
  heap-owned reader is created. File validation and full-table row scans
  repeat per scope and query respectively; this is a bounded fallback. The
  complete preparation/query stack still needs qualification. No per-pin
  heap, new worker or additional arena is admitted.

  The Rust allocation probe measures complete pinned read scopes in sixteen
  intervals at short and maximum roots. Cases cover successful get/next/change
  reads, an append during an old reader callback, deadlines before preparation,
  after overlay loading or at callback completion, work/scratch refusal and
  ignored query errors. Each interval also retries the old pin and queries a
  fresh pin, then drops both. Selection loading, captured-prefix replay,
  table/history validation, query I/O and temporary disposal run inside
  measurement. Fixtures, buffer allocation, owned verification, session startup
  and owner teardown remain cold. Every counter, including frees and peak,
  must remain unchanged. These small empty-final-state and short-history
  fixtures do not qualify maximum data, native allocation, worker stacks or
  whole-service RSS.

  ReadScratchSlot checks exact admitted replay-byte/cell and change-cell
  extents before clearing all backing at startup. ReadScratchPool
  exclusively borrows exactly storage_views slots and moves a partition into
  one private lease per capture. Per-view bookkeeping reserves 1 KiB from
  the existing cursor/value allowance: slot at most 256 bytes, pooled reader
  at most 512 bytes, and pool at most 64 bytes (conservatively charged per
  view). Runtime constructors enforce those compiled layout ceilings. No
  arena size or total changes. Capture is a bounded try-lock scan; drop
  briefly locks to return backing, without resetting poison. No lock spans
  query I/O or callbacks. Complete worker stacks, service startup and
  whole-service RSS remain unqualified. Pool allocation measurements follow
  below.

  The Rust allocation probe measures sixteen pool intervals at short and
  maximum roots: normal queries, append during an old reader callback,
  initial deadline, zero-step work refusal after preparation, pool capacity,
  slot contention, publication contention and publication capacity. It calls
  public capture and pooled queries, retries the old reader, reads a fresh
  pin, and drops both leases before the closing snapshot. Every Rust
  allocator counter must stay unchanged. Both full-size scratch sets are
  allocated and touched before measurement; fixture creation, owned
  verification, frame creation, pool/session startup and owner teardown
  remain cold. Query data remains small and empty in its final state, with
  short retained history. Poison, unwinding, native allocation, maximum
  datasets and complete worker stacks are outside this probe's measured
  scope.

  A pooled view lends at most one PinnedBlobInput or PinnedBlob at a time.
  Their combined compiled state is capped at 4 KiB during construction,
  reserved from existing cursor/value space. This covers the descriptor,
  digest, clock and terminal-error state across the consuming handoff. A
  fixed 64-byte BlobRow lookup buffer uses the worker stack. Sequential and
  random reads use caller output and at most 64 KiB per call, without a
  whole-body arena. The descriptor retains the pooled view's pin and backing
  until drop. No arena size or planning total changes; complete worker-stack
  and whole-service RSS qualification remain separate.

  The Rust allocation probe measures twenty body intervals at short and
  maximum roots. It covers normal and empty bodies, deletion after capture,
  missing-row/byte-cap refusal, checksum mismatch, truncation and deadlines
  after input read, digest finish or random read. Public pool capture, row
  lookup, body opening/hash/finish/range reads, terminal retries, post-error
  metadata queries and pin disposal are inside measurement. Require exactly
  forty snapshots and every Rust allocator counter unchanged. Fixtures, full
  scratch creation/touching, owned verification, session startup and teardown
  stay cold. A preopened fault handle truncates inside its measured interval;
  checksum corruption is installed before measurement. These zero/three-byte
  fixtures do not qualify maximum body sizes, native allocation, clock-source
  or regression failures, constructor I/O faults, worker stacks or service RSS.

  HistorySweep retains one HistoryChangesInput and selected metadata/scalars;
  its shipped-Provider state fits 2 KiB on the worker stack. Completion moves
  that input to a temporary; complete worker-stack qualification remains pending.
  Existing record scratch and full change slots are reused across segments.
  Descriptor byte/frame totals are admitted before I/O; each frame retains the
  operation reader's existing work bound. No arena or journal buffer is added.
  Allocation probes cover selected completion and slot reclaim at both roots.
  Frame-change collection has its own 4096 slots of at most 24 bytes each,
  a separate 96 KiB reservation per view. Retained changes may coexist with
  get/next result storage; their memory never aliases those partitions. This
  adds 192 KiB for the default two views within the unchanged 96 MiB budget.
  Limits::plan checks the compiled Cell layout. Collector retains only the
  frame verifier, slot borrow and scalar progress; its compiled Provider layout
  is capped at 1 KiB on the worker stack. Operations use the existing record
  region, and copied CHANGE data survives reuse of that input. The allocation
  interval covers maximum-capacity collection, repeated draining, corrupted
  footer and short-slot failure after cold slot/input preparation. Runtime
  cursor I/O, whole-worker stack/RSS and actual pin ownership remain pending.
- Writer: one journal arena and descriptor array, one 1 MiB frame, and
  256 KiB table/manifest/value scratch, a separate 1 MiB input key/value
  arena and 4096 operation slots of 32 bytes. The latter bounds the actual
  Option<StagedOperation> layout: owned offsets, lengths, type tag, ordinal
  and PUT/DELETE/CHANGE kind/action. All operations share that one index;
  there is no second CHANGE descriptor array. Journal descriptors are a
  different structure. Limits::plan
  checks the compiled operation-index layout fits. Decode one borrowed row at
  a time from the input bytes while encoding the separate output frame; never
  retain a self-referencing row array or alias input with mutable output.
  Pending commits reference fixed protocol slots and have no private frame.
  Exclusive GC reuses 4 KiB of writer scratch for 128 candidate cells; it
  cannot allocate an inventory-sized live-ID set.
- Outbound: exactly 100 envelope cells of 320 bytes, 100 distinct 4096-byte
  RCPT reply cells, and 128 KiB transfer/reply scratch. This per-attempt batch
  is independent of the configured inbound recipient ceiling. The final DATA
  reply is shared by its subset; encoding copies it into each recipient row.
  Prior persisted replies are reread into writer staging when needed, not
  retained as a second per-recipient array. V1 fixes outbound_deliveries at
  one; online ACME borrows that slot under the scheduling contract below.
- Sort scratch: one shared MiB for run formation and bounded merge buffers.
  Disk spill space is independently capped at 64 MiB by default.
- Fixed control scratch and queues have the partitions below. No HTTP-01,
  administration, resolver, ACME or migration path can invent another pool.
- Eight worker stacks fund the fixed roles below; no worker/thread may appear
  outside this count without a ledger amendment. The main stack allowance is a
  resident budget,
  not a claim that the host's virtual stack mapping is one MiB. The control
  worker reserves 80 KiB of its existing 256 KiB stack for temporary borrowed
  identity views, leaving 176 KiB for all other live frames/copies/provider work;
  actual target layouts and peak stack usage must pass before this path runs.
- TLS sessions include SMTP, HTTPS and outgoing delivery slots; handshake
  scratch is additional. The handshake cap is global in this profile.
  TLS wire assembly and socket-write tails are charged within those entries;
  each framed-record buffer reserves 18437 bytes. The facade's strict outer
  bound rejects bodies >=18432; negotiated limits may be smaller. The main-loop 16 KiB chunk
  limit below refers to application plaintext; one ciphertext-record turn
  includes its bounded TLS overhead. It is not a 16 KiB wire-buffer promise.
  M07d2's record pump retains two distinct 18437-byte reservations per
  connection (36874 bytes total), one input frame and one output/tail frame.
  These count against the 512 KiB session target, not extra headroom.
  Before the final handshake flight drains, the facade permits 83973
  ciphertext bytes. Its conservative sum with 16384 plaintext bytes and
  36874 pump bytes is 137231. This window can extend past authenticated
  Finished. The mail owner retains its handshake permit until facade and
  socket output drain;
  the corresponding additional handshake memory reservation must cover the
  whole pre-drain window before activation. After authenticated Finished and
  complete facade output drain, its permanent ciphertext ceiling is 36874 bytes. That established
  ceiling plus 16384 plaintext bytes and the pump buffers totals 90132 bytes,
  leaving 434156 of the 512 KiB session target before handles, retained input,
  peer chains, native state and allocator overhead. Post-operation output
  refusal does not bound temporary allocation or Vec capacity. Complete
  session accounting remains an M07e admission blocker. Measure coexistence
  within 512 KiB, or amend the ledger before serving. The runtime must reserve
  the wire buffers before admission and recover both through into_buffers on
  teardown or constructor refusal, for either representation. The pump accepts
  borrowed references or Box-owned arrays allocated before admission; it never
  allocates or replaces them. Ordinary Drop frees owned storage and is not a
  pool-return mechanism.
  Count each buffer once regardless of its ownership representation; owned
  allocation bookkeeping remains part of the same session target.
  M07d3a's service-global HandshakePool reserves only the scalar handshake
  count. Charge its cold shared bitmap allocation and live permits to the
  71,168-byte remainder of the existing 128 KiB Slot queues and queue window
  reservation; each live permit fits 16 bytes. Dropping a
  permit returns capacity with one atomic operation, without taking a lock.
  Reserve/drop make no allocations. It does not reserve the session buffers
  or native handshake allowance; the admitting factory must couple all of
  them before 220. Startup must create one pool shared by all handshake workers,
  never one pool per listener or worker. Complete allocation/RSS accounting
  remains M07e.
  Internal backend queues, peer chains and retained handshake fragments need
  separate measurement within these same entries, not an added allowance.
  Incoming TLS 1.3 tickets still derive secrets, query the clock and copy peer
  chains before discard with resumption disabled. Include these temporary
  allocations and record work in established-session measurements.
  Each admitted handshake has an additional 4 MiB allowance for provider
  construction, processing and retained handshake state. The main thread has
  one separate 4 MiB established-processing allowance: it executes only one
  TLS operation at a time, including control-message decoding after Finished.
  It can coexist with both handshake workers and both certificate generations.
  Increasing connection counts does not multiply this serial allowance;
  moving established TLS work to more threads requires a ledger amendment.
  These allowances fund provider allocations, not extra td-owned wire pools.
  Raw handshake byte caps do not bound decoded vector sizes. The decoded-list
  fixture charges pre-Finished expansion to handshake headroom. Incoming
  tickets demonstrate that provider processing also occurs after Finished.
  HTTP-01/administration must use the existing fixed control/I/O reservations;
  they cannot silently add another general connection pool.
- Each 8 MiB certificate generation includes every server profile and parsed
  relay/ACME/gateway trust object, retained raw bytes and allocation overhead.
  SCHEMA.md's per-input caps do not enlarge this aggregate. Renew a complete
  generation, retaining at most old and replacement; no per-profile side cache.
  M07d3b1's immutable gateway policies, owned names/pins/prefixes and shared
  server configurations belong to this same entry. The temporary TrustStore
  drops after configuration construction; no second persistent trust cache is
  retained. Fingerprinting uses fixed cold scratch (16 KiB certificate decode,
  4 KiB anchor digests and eight canonical prefixes) on the existing control
  stack. Its complete stack/native allocation peak remains unqualified until
  M07e; input count caps do not prove the generation or stack allowance.
  M07d3b2a's explicit-startup GenerationSet counts current, reserved, candidate
  and retained old owners against two shared slots before invoking a loader.
  The detached reservation permits control-worker construction without holding
  a main-thread lock. The loader returns Box<T>, avoiding a by-value T handoff
  through that worker's stack; its own stack use remains bounded by the loader.
  Boxed payloads are freed
  before their slot is released. Charge the cold bitmap, payload/shared-owner
  allocations, leases and briefly overlapping final-drop ownership headers to
  this same ledger, including allocator overhead; no new allowance is added.
  The runtime bounds lease counts and destruction concurrency with its fixed
  slots/workers. GenerationSet cannot bound T's contents, loader temporaries,
  extracted resources or native allocations; M07e still proves their aggregate.
  The TLS policy compiler reserves at most 18 table entries (16 listeners,
  relay and ACME). Within one complete compilation it reserves at most 32
  temporary HTTPS identity cache entries: one per certificate profile with and
  without the JMAP origin binding. Equal views share the same narrowed identity
  across listeners; empty views are cached too. The cache is dropped before
  publication, while each server configuration retains its selected shared
  identities. Listener policy bindings remain distinct. This cache belongs to
  the existing generation/cold construction allowance, never a separate pool.
  The compiler constructs each full identity once and shares it among native
  configurations (HTTPS/gateway subsets share admitted keys and chains),
  reads one gateway bundle for all its listeners, and validates
  unused staged gateway bundles without retaining a trust cache. Per-input
  raw buffers allocate their ceiling plus one overflow byte cold; chain and key
  coexist during identity admission, and one CA buffer coexists with its parsed
  configuration. Raw input windows are cleared on drop without a secure-erasure
  claim. Table vectors, strings, boxed gateway policies, native objects and all
  temporaries remain charged to this same generation allowance. Explicit
  client-only preparation reserves at most two table entries and opens only
  relay/ACME trust. It occupies one of the same two generation slots; complete
  replacement and retained client sessions cannot create a third slot or
  independent trust-cache allowance. The compiler
  does not yet measure/enforce the 8 MiB native aggregate and cannot activate
  serving. SessionPreparation retains the handshake count, generation and two
  preallocated buffers without allocating; native construction runs later on a
  fixed TLS worker. The runtime must still supply/retain the complete session
  slot and native byte allowance; a pair of arrays alone is not that admission.
  Socket handoff moves those same owners into TlsConnection without another
  wire allocation. It holds the count permit through queued/Pending native
  progress, releasing it only after mail handshake authorization or abort.
  The generation remains retained until native pump destruction, including
  after a failed connection awaiting buffer recovery. Charge wrapper fields
  and optional-permit bookkeeping to the same session/slot reservations.
- Certificate overlap, reload overlap, allocator bookkeeping, executable
  pages and main/worker stacks all count at peak coexistence. RSS tests must
  validate the allowances; this ledger is not an OS memory limiter.

Message size, upload/queue quotas, queue length and log file limits are disk or
admission bounds. Growing them does not reserve whole bodies or a whole queue in
RAM. The default log disk reservation is five 8 MiB files (active plus four
retained). ADMISSION.md freezes logical byte/file quotas, logical completion
reserves and maintenance work/deadlines, enforced by M05/M08 before mail can
be accepted.

The per-upload byte ceiling is `message_bytes`, initially 32 MiB; M13 publishes
that value as `maxSizeUpload` and enforces it even for attachment uploads.
`upload_disk_bytes` is the separate aggregate quota for retained upload blobs.

The compiled maxima and checked default values live in `limits.rs`; this table
records the default byte ledger only. Journal/frame limits are fixed to the
storage contract. SMTP retains room for at least 100 recipients. Disabled event
streams may use zero slots; mandatory pools and byte budgets cannot be zero.
The bounds are startup validation, not protocol error mappings or proof that
all combinations meet standards. ADMISSION.md fixes operation work limits;
POLICY.md adds parser/search field limits. M13 publishes only limits that its
admission code actually enforces.

## Fixed execution ownership

These are implementation requirements, not running workers. The main thread
owns nonblocking accept/read/write and bounded established-TLS record work.
It visits connection slots in rotating order, at most one 16 KiB application
chunk/record per ready slot per turn. It then processes completion/timer events
and waits on a notified condition variable for at most five milliseconds or
the nearest deadline. Always use the earlier time. No poll/epoll unsafe adapter
is added. Socket blocking, DNS, disk operations, config parsing and log writes
cannot run on the main thread. Short pool locks use nonblocking acquisition
there; unavailable work stays queued within its deadline.

| Fixed worker | Count | Work and ownership |
| --- | ---: | --- |
| Writer/checkpoint | 1 | Serialized metadata planning/validation, frame sync/publication and checkpoint barrier; owns transaction staging |
| Body/read | 2 | SMTP/HTTP parsing, JSON tokenization, auth verifier work, JMAP dispatch/serialization, streaming blob I/O, MIME, reads/search and per-object planning; yields by bounded chunks |
| TLS handshake/dial | 2 | Bounded connect_timeout and handshake steps; moves a transport slot back to main after success |
| Logger | 1 | Fixed event queue drain, bounded encoding and file rotation |
| Resolver | 1 | A/AAAA DNS packet I/O, TTL cache and CNAME parsing with fixed deadlines |
| Control | 1 | Configuration/device/ACME/administrative jobs and backup coordination |

A configured larger body-job or handshake pool increases resident job slots,
not thread count. Workers resume bounded steps across those slots. A waiting
network peer never occupies a worker while idle after dial: retain state in its
existing slot and return Pending to main. The explicit exception is std's
blocking connect_timeout: only the sole outbound slot may dial, so at most one
of the two handshake workers is occupied by it. The other remains available
for inbound handshakes. A pending handshake is dispatched again after ten
milliseconds, at most one queued/running step per slot. Each step does bounded
nonblocking I/O/crypto and returns one completion; it never waits for socket
readiness. The compiled eight-handshake maximum thus admits at most 800 such
steps/second, within the fixed queues and global completion-credit rule below.
This polling cost is an unmeasured M07/M23 acceptance obligation.
Disk I/O may block its fixed worker; it cannot
create replacement threads. Deadlines prevent accepting more work behind a
failed/stalled resource, but cannot promise cancellation of a kernel I/O hang.
An unexpected worker failure stops new mutations and fails health; no panic
catching is used as transaction recovery.

Queues carry slot ID + checked reuse generation, work kind and bounded scalar
arguments, never messages, unbounded closures or cloned response trees. Only
one worker/main owner may mutate a slot at once. Transfer ownership through a
fixed queue; stale completion generations are rejected. Every enqueued job
reserves one completion credit before effects. Its one success/error/Pending
completion consumes that credit, held until main drains it. The sum of queued,
running and undrained completed jobs never exceeds 512; dispatch refuses or
waits before effects when credits are exhausted. A durable result cannot be
dropped or relabeled failed because its output queue is full. Re-dispatch of
Pending requires a new credit. Queue saturation returns a typed temporary
error before side effects. Do not use blocking send from main or hold a pool
free-list lock across I/O/another queue wait. An exclusively owned per-slot
lock may span its worker's I/O; main uses try_lock and skips busy slots.
Writers never wait
for body workers while holding the commit lock. Body publication completes
before the metadata request enters that lock; reservations remain independently
charged while their slot waits. Checkpointing uses the writer's own buffers.

Body/read workers advance protocol CPU work by at most 16 KiB input/output or
256 parser/token/object transitions per step, whichever comes first. Parsing a
whole request is not one main-thread operation. Main only does bounded socket
record work, scheduling, and small HTTP-01/event/health framing from existing
validated state (at most 2 KiB of that control framing per turn). Worker jobs
resume in rotating slot order; no request owns a worker across an idle wait.
Use fixed Mutex/Condvar rings, not channels with implicit growable wait queues.
Use allocation-free unstable sorting with a complete deterministic tie-break,
never a stable sort that allocates hidden scratch. Typed adapter errors carry
bounded codes, not newly allocated error messages. M05 must measure std path
conversion/directory iteration and impose a verified path bound or explicitly
account for unavoidable adapter allocations; no unverified std stack threshold
is part of this contract.

Event streams use main's normal nonblocking output scheduling and one coalesced
state notification per slot; they hold no read view or body worker between
emissions. Health uses a bounded cached snapshot. The control worker can update
that snapshot without making every health poll wait for an ACME/DNS request.
The diagnostic encoding bounds below assign cached encoded output, two fixed
observation/output pairs and a minimal unavailable frame within that region.
Only one sort/search job leases the shared sort buffer at a time. Long background
views share one permit under ADMISSION.md: backup and outbound body transfer
cannot together occupy both default views. Backup uses an existing view and
control coordination, preserving one foreground view. Extra requests wait;
they do not allocate replacements.
An online configuration enabling backup/outbound background views requires
storage_views >= 2; one-view foreground/offline profiles remain valid.
The fixed 64 reservation records are a shared
admission cap, not one guaranteed record per configured connection. Connections
without a record wait/refuse before body or metadata effects; each outbound
attempt must acquire its phase and outcome records before the acceptance fence.
Larger connection pools do not imply larger reservation capacity.

The one outbound transport slot serves either a queue attempt or ACME HTTPS.
Reserve it before DNS/dial; ACME does not add an eighteenth
TLS session. V1 allows one outbound SMTP transaction at once. A control lease
lasts at most 60 seconds; release it between ACME polling waits.
When both classes wait, alternate a queue attempt and a
control lease. This schedules opportunities, not a promise of successful
network progress. ADMISSION.md fixes every lease's idle/total deadlines.

Migration is an offline CLI operation holding the exclusive store lock, not a
job in the running service. It instantiates the same checked ledger with one
outbound slot and HTTPS request/token arenas for JMAP source pages, with no
serving listeners or queue dispatch. Its source-response ceiling is json_bytes
and json_tokens; page sizes must fit, and an overlarge page is an explicit
failure. Raw blobs stream under message_bytes. ADMISSION.md fixes finite transfer
deadlines suitable for an offline import rather than ACME's 60-second lease.
M21 must measure this separate process and its bounded page/body lifecycle.

## Scratch partitions

Partitions are byte ceilings at simultaneous peak. Implementers may reuse
space only when the lifetimes are mutually exclusive and tested. Larger
concrete structures require a ledger amendment before admission is enabled.

| Reservation | Partition |
| --- | --- |
| Body work, 96 KiB/job | Six 8 KiB nested-decode rings (NestedPartId::MAX_STEPS); 16 KiB parser/boundary/locator state; 32 KiB conversion/output |
| Read cursor/value, 292 KiB/view | 68 KiB table-record input; 64 KiB retained result value; 1 KiB key; 5 KiB selected metadata; 1 KiB pool bookkeeping; 4 KiB pinned-body input/reader state; 53 KiB cursors, history streaming, sparse-index lookups and checksums; 96 KiB retained frame changes |
| Outbound scratch, 128 KiB | 64 KiB body transfer; 16 KiB reply assembly; 16 KiB SMTP/TLS handoff state; 32 KiB frame-planning/ID/diagnostic scratch |
| DNS/control, 512 KiB | 128 KiB resolver + 384 KiB control as detailed below |
| Log, 128 KiB | 384 queued fixed event cells of at most 256 bytes (96 KiB); 16 KiB encoder/output; 16 KiB rotation/drop counters and emergency status |
| Cold reload, 2 MiB | Two immutable configuration snapshots of at most 1 MiB each, including referenced credential data; reject a third live generation |

The resolver's 128 KiB includes 128 cache entries of at most 384 bytes
(48 KiB), a 64 KiB packet/TCP buffer and 16 KiB question/name/alias-chain,
address-result and cursor state. Cache keys are configured endpoint index plus
configuration generation, not copied DNS names/CNAME chains. SCHEMA.md selects
1..4 explicit numeric resolver endpoints and restricts ACME operational URLs to
the directory origin. Offline migration may own one additional bounded endpoint
slot for its explicit source origin, released at job exit. Entries retain
only the final address set and minimum chain/address TTL. A job returns at
most 16 addresses. Do not
allocate one packet/name buffer per waiting lookup. Expired entries are
replaced in place. Configuration fixes names/resolvers; this is not a recursive
resolver or a DNS cache for incoming arbitrary domains.

Control has six 64 KiB regions: HTTP transfer, JWS/CSR, JSON input/tokens,
configuration stream scratch, administrative output, and main-owned control
state. ACME JSON input is at most 32 KiB with 2048 16-byte token slots in its
64 KiB region; non-JSON certificate bodies stream through transfer storage
into the separately budgeted certificate generation. Main-owned control state
includes two 8 KiB HTTP-01 slots, an 8 KiB Unix-command input slot, a 16 KiB
health snapshot and 24 KiB descriptors/challenge data/counters. HTTP-01 emits
from the existing validated challenge bytes without occupying a worker. It
has no general body upload path. The control worker uses its other five
regions serially; main never borrows a region still owned by that worker.
HTTP-01 allows one slot per peer, one request per connection, and a five-second
total lifetime. When full, a new peer may replace the oldest slot that has made
no progress for one second; otherwise refuse it. Close after the response.
This bounds simple slot pinning, not distributed denial of service.

The six decode stages count nested transfer-decoding boundaries, separately
from MIME structural depth. WIRE.md already refuses a seventh stage as
notParsable while preserving raw download. MIME descriptors do not each own
a decoder ring. POLICY.md freezes the input decoding details. Each 8 KiB stage
reserves 6 KiB for bytes and 2 KiB for eight checkpoints of at most 256 bytes.
A checkpoint records encoded/decoded positions and small decoder state; it
never copies a ring. Retain the checkpoint before each buffer fill and those
needed by the at-most-six nested lookahead users. Restoration discards buffered
bytes and replays from the saved source state, not from the root file origin.
Short whitespace runs use their resident ring directly. Long-run QP lookahead
restores the whole bounded source-state chain and resumes after the known run;
it cannot repeatedly search from the same position. M06 must prove linear
replay in run length at the fixed six-stage ceiling, including refills and
simultaneous nested lookahead. Every replay byte/transition is charged.

The initial MIME base64 decoder keeps at most 32 bytes of inline state,
including at most three pending output octets. It borrows caller buffers and
yields after 256 input/output/EOF transitions. Source visits and emitted
bytes consume the enclosing work meter; record charging remains with MIME
object traversal. Its copied state fits inside a future source-position
checkpoint rather than copying any ring. No arena or process-budget total
changes. The Rust allocation probe covers construction,
empty/normal/malformed input with single-byte output, completion and sticky
work refusal in one interval with every counter unchanged. This does not
qualify nested sources, maximum bodies, complete parser stacks or service
RSS.

The quoted-printable cursor fits 64 bytes of copied stage decoder state,
including extent/run positions and at most two pending octets. It scans long
whitespace runs without buffering and replays interior runs once from their
exact start. Runs still present in the supplied fragment replay there within
the same bounded turn; only earlier positions require external reposition.
No line/body buffer or resource allowance grows. Its isolated
allocation interval covers soft breaks, interior/trailing whitespace,
malformed escapes, repositioning, short output, copied-state replay and
sticky refusal. The raw transfer reader now retains that state in a fixed
decoder enum and
its saved slots; no extra backing is added. Nested decoded-source ownership
and simultaneous checkpoint scheduling remain separate.

The first transfer-input owner borrows a BlobReader and at most the existing
6 KiB stage byte partition. Its inline compiled state fits 256 bytes; it
retains no extra body arena or thread. One poll refills that partition or
performs a 256-transition decode turn, with one shared clock watermark and
charged requested I/O plus resident byte visits/output. A synthetic-source
allocation interval covers identity/base64 construction, short output,
completion and sticky work refusal with every Rust counter unchanged. A
separate functional test uses the real verified pinned body reader. These
checks do not qualify combined filesystem allocation, full worker stacks or
service RSS. Optional Checkpoints backing holds eight private saved positions
and decoder states fitting the existing 2 KiB stage checkpoint partition;
each saved state is at most 256 bytes and the complete backing at most 2 KiB.
The live Reader still fits 256 bytes. Binding clears old slots; save/restore
copy no ring and allocate nothing. A synthetic-source allocation interval
covers binding, save, replay, completion and sticky refusal. Nested source
chains and their simultaneous checkpoint use remain unqualified.

QP integration retains one additional raw extent origin, a fixed decoder
enum and the existing backing. Reader still fits 256 bytes, each saved state
fits 256 bytes and eight slots fit 2 KiB; the text owner still fits 512
bytes. The QP input interval covers resident reuse/refill, checkpoint
replay, source failure and two-pass charset decoding with unchanged Rust
counters. Functional tests restore at every turn and inject every clock
boundary plus a rewind read failure. Long prose, bare CR and whitespace
cases bound physical reads and total charged bytes with full 6 KiB backing.
These are synthetic-source measurements, not combined filesystem,
nested-source or complete-worker qualification.

The raw header scanner uses at most 128 bytes of inline state within parser
state and returns one 32-byte field descriptor at a time. It holds no source
buffer or per-field string; long tentative names consume charged work with
constant memory. The caller's existing header arena owns any retained bytes.
A dedicated allocation interval covers single-byte fragmentation, folded
field emission, body detection and sticky limit refusal with unchanged Rust
counters. This does not qualify a full parser stack or source collection.

The resident header occurrence cursor fits 384 bytes in the body job's
16 KiB parser/boundary/locator state, including the raw scanner, borrowed
property and two optional field descriptors. Header bytes and requested
names remain in their existing externally owned arenas; no name, value or
match list is allocated. All matches stream provisionally; last selection
retains one extent. Isolated allocation intervals cover last/all, absence
and long matching names. Complete capture and worker stack remain open.

Aggregate header selection (M06ar) reuses the 384-byte selector and 128-byte
scanner ceilings, with one private byte of prepaid step credit and a temporary
borrow of the existing email budget and job meter. It allocates no backing.
Each turn charges at most 255 source visits, 256 aggregate steps and 16 job
records. Repeated lookahead and both name-comparison operands count; transitions
without input still charge steps. Credit is never copied into checkpoints or
refunded. The allocation probe covers repeated long-name selection through
aggregate refusal. Other interpretation stages still need shared-budget
composition.

The structured CFWS cursor fits 64 bytes in the body job's 16 KiB
parser/boundary/locator state. A depth counter enforces the 32-level comment
nesting limit; no recursive stack or comment collection is allocated.
Returned extents describe the immutable source using offsets. Allocation
intervals cover long UTF-8 comments, folding, nesting and malformed/depth
refusal. The complete structured-form parser stack remains unqualified.

The delimited-token cursor fits 64 bytes in that same parser reservation.
An explicit kind and escape state handle either a quoted string or domain
literal; returned raw extents borrow no new storage. No nesting stack or
copied token is retained. Allocation intervals cover long UTF-8 values,
escapes, malformed input and work refusal for both kinds. The complete
address/MessageIds parser stack remains unqualified.

The MessageIds list cursor fits 256 bytes in the body parser reservation,
including its CFWS/delimited child states and raw source offsets. Long atoms,
phrases and lists do not grow state. Allocation intervals cover Unicode and
quoted data, obsolete phrases, malformed tails, nesting and work refusal.
Returned byte extents allocate no output. The enclosing response owner must
reserve its own projection storage and qualify complete worker composition.

The validated MessageIds projector fits 384 bytes in the same body parser
reservation, including the raw parser, unfolding/UTF-8 states and one-byte
handoff. It charges both complete validation and replay, and emits
individual scalars without retaining identifiers. Intermediate unfolded
bytes and final UTF-8 lengths both consume output work. Long ASCII atoms
cost about 5.06 records per byte plus field overhead; permitted header size
does not guarantee fitting the remaining foreground work (API section 1.35).
Allocation intervals cover long Unicode text, folds, noncharacters,
malformed tails and output refusal. Response storage, JSON escaping and
complete worker composition remain the enclosing owner's responsibility.

The URLs cursor fits 256 bytes in the body parser reservation, including
CFWS and URI states plus 45 bytes for an IPv6 literal. Two charged passes
validate then replay individual ASCII URL bytes without retaining a list.
The bounded std IPv6 parser adds 64 prepaid records per literal per pass;
IPvFuture and arbitrarily long names/paths retain fixed state. Allocation
intervals cover long URLs, IPv6 and IPvFuture, whitespace, malformed input
and refusal. Complete JMAP response storage and worker composition remain
unqualified.

The address recovery boundary cursor fits 64 bytes in the body parser
reservation, with fixed quote/literal/escape flags and separate 32-level
comment/angle counters. Raw item offsets retain no text. A complete scan
charges one record per byte plus EOF; no output is charged. Allocation
intervals cover long quoted UTF-8, domain literals, unmatched tails, empty
items, nesting limits and work failure. Complete address/group grammar,
text/NFC composition and response storage remain unqualified.

The single addr-spec wrapper fits 288 bytes in the body parser reservation,
including the shared identifier parser and private error latch. Its bare
mode adds no source buffer, synthetic delimiters, arena or duplicated
local/domain grammar. Public MessageIds state remains within 256 bytes.
Returned raw parts charge no output. Allocation intervals cover long UTF-8,
quoted folds, malformed tails and nesting/work refusal. Mailbox/group and
text/NFC composition remain to be qualified.

The phrase token cursor fits 320 bytes in the body parser reservation,
including shared word/CFWS state, source/extent bookkeeping and its error
latch. Its private purpose adds no copied display name or arena. Exact
leading/trailing CFWS extents preserve later encoding-placement context.
Token classification adds one prepaid byte visit and record to the shared
core's turn, for ceilings of 161 bytes/33 records and no output charge.
Allocation intervals cover long Unicode atoms, quoted folds, obsolete dots,
malformed tails and nesting/work refusal. Complete mailbox/group and
text/NFC composition remain to be qualified.

The single-mailbox cursor fits 512 bytes in the body parser reservation.
Its enum retains only one active CFWS/delimited/phrase/domain/addr-spec
child, with scalar scan/route state and result extents. Names, routes and
addresses are never copied. Each poll charges one parent record plus at
most one child turn, at most 161 byte visits/34 records and no output.
Structural scanning and comment replay share the live meter. Allocation
intervals cover long names/addresses, routes, quoted folds, fallback names,
malformed tails and nesting/work refusal. List/group recovery, text/NFC
composition and complete worker stack qualification remain open.

The address/group assembly cursor fits 768 bytes in the body parser
reservation, including the 64-byte recovery boundary cursor and one active
CFWS/phrase/mailbox child. First-colon metadata adds no copied token to the
boundary cursor. Group names, pending mailbox results and raw fallbacks
are offsets/scalars; there is no item/group collection. Each poll adds one
parent record to at most one child turn, for 161 byte visits/35 records,
with no output charge. Fallback edge trimming is charged one byte at a
time. Allocation intervals cover groups, null slots, routes, long Unicode,
raw recovery and resource refusal. Projection storage and the complete
composed worker stack remain unqualified.

Validated phrase proofs hold a borrowed slice within 16 bytes. Their Copy
replay cursors fit 80 bytes with scalar offsets, lexical/nesting/escape state
and an error latch; they hold no meter or output arena. Each poll prepays at
most 32 source-byte visits and 32 records. Every repeated traversal charges
the live meter again, including peeks and EOF; offset events spend no output
budget. Cached completion is inert and failure survives copying. Allocation
intervals cover proof creation, Unicode/quotes/comments, checkpoint copies,
replay and sticky refusal. Integration into the fixed NFC Source allowance
and the complete worker remains unqualified.

The phrase display-name decoder fits 224 bytes. Its enum stores either
token replay or encoded-word decoding, with a private source-bound resume
checkpoint and scalar offsets/flags, including a turn ordinal for future
NFC checkpoints. It retains the admitted field for exact placement context;
names are never copied. Literal UTF-8 uses a four-byte stack array. Each
poll charges at most 230 byte visits and 227 records, including charged
classification, quote trimming, lookahead and copied traversal. Scalar
output charging belongs to the enclosing serializer. Allocation intervals
cover the complete phrase composition and refusal. NFC Source/whole-worker
integration must still demonstrate their existing size and work ceilings.

Phrase display-name NFC retains the existing 3072-byte Scratch and at most
1024 bytes for cursor plus HeaderBudget. Each private source checkpoint fits
256 bytes including phrase decoder state and pending canonical expansion.
Checkpoints compare exact field/range/turn identity without prefix scans.
Each phrase poll charges at most 231 aggregate steps and 15 job records;
all decoding/replay visits debit the same live budgets. The maximal one-MiB
ASCII phrase fits default limits; hostile-tail replay does not revisit a
finished prefix. Allocation intervals cover full decoding/normalization fast
and overflow paths. Caller-owned grammar/whole-field aggregate admission,
response storage and complete worker stack/RSS remain unqualified.

M06an's exact comment validator fits 96 bytes and its proof 16 bytes. It
retains one CFWS child and charges at most 160 visits/33 records per poll.
The Copy comment scalar decoder fits 192 bytes; its enum retains only one
encoded-word decoder, with fixed UTF-8 scratch and one held scalar. Each poll
fits 225 visits/227 records, with owner output charging. No name length or
nesting count grows memory. NFC retains its 256-byte Source, 3072-byte Scratch
and 1024-byte cursor-plus-budget ceilings, within 231 aggregate steps/15 job
records per poll. Copies contain no live meter or credit. Allocation intervals
compose validation, scalar conversion, normalization fast/replay paths and
refusal. Whole-worker memory, native allocations and service RSS remain
unqualified.

The normalized scalar JSON adapter fits 64 bytes of framing state and
exclusively borrows NFC's separate 4 KiB conversion reservation. It retains
six pending escaped bytes and uses a four-byte UTF-8 local; response storage
is supplied by its owner. Each turn performs one source poll or one fixed
framing/drain action, copying at most six bytes. Escaped output is precharged
once even with one-byte slices. Allocation intervals compose normalization,
fragmented serialization and refusal; retained response storage and combined
worker stack/RSS are not qualified by this helper.

The JSON adapter also borrows Raw or parsed/fallback address sources through
one fixed enum. Its complete wrapper still fits 64 bytes, with no owned source
parser and no meter copy. These modes borrow no NFC scratch. Raw state remains
in the 2 KiB decoder/HTML/snippet state of the 32 KiB conversion region; the
address facade remains in the 16 KiB parser reservation. Original source
conversion charges and escaped JSON output charges both apply, while
fragmenting a staged JSON scalar adds no charge. Allocation intervals cover
Raw, both address modes and malformed-source refusal. No complete
response/worker claim follows.

The aggregate Raw owner (M06as) fits 96 bytes including its decoder state,
references to existing email/job budgets and private step credit, within the
existing 2 KiB decoder/HTML/snippet reservation. It is not Copy or Clone and
allocates no scratch. Each scalar poll uses at most four source visits, six
aggregate steps and one prepaid job record. Its borrowed JSON adapter remains
within 64 bytes; zero-byte live checks and output charges consume no
interpretation credit. Allocation intervals cover long one-byte serialization
and terminal output refusal. Whole-worker stack, native allocation and RSS
remain unqualified.

The Raw property coordinator (M06at) fits 640 bytes in the 16 KiB parser-state
reservation, including occurrence selection, one budgeted Raw owner, JSON
Frame and four literal bytes. It owns no header, property-key or output arena.
This path moves the Raw state into the coordinator; it does not simultaneously
use the standalone Raw state in the 2 KiB decoder reservation. Each turn calls
at most one bounded child plus fixed ownership/literal actions, copying at
most six output bytes; aggregate bounds stay 255 visits, 256 steps and 16 job
records. JSON chunks use the HTTPS slot's existing 16 KiB output region and
belong to an unpublished response-spool tail; there is no whole-value memory
reservation. Allocation intervals cover long values, live budget handoffs and
late refusal. Spool I/O and combined worker/native/RSS qualification remain
open.

The Text property coordinator (M06au) fits 1664 bytes in the existing 16 KiB
parser-state reservation, including one inline NFC cursor and the shared
selector/framing state. Raw remains within 640 bytes. Text borrows the existing
3072-byte NFC scratch. The standalone 1 KiB cursor slot in the 4 KiB NFC
region is unused on this path; the inline cursor lives in parser state. Generic
projection selection adds no trait object, allocation or second source. Turn
bounds remain 255 visits, 256 aggregate steps, 16 job records and six output
bytes. Allocation intervals cover overflow normalization, scratch reuse for
another field and late refusal. M06bj's Content-Description/X- admission
adds no retained state. With M06bm's structured fields and M06bn's MIME
parameter fields, admission costs at most forty name visits, three steps
and three records before any JSON. Existing source/turn bounds hold, and
allocation intervals include all admitted grammars. Combined worker
qualification remains open.

M06bp's shared CFWS and delimited-token cursors replace the existing inline
lexical owners; their mail facades still fit 64 bytes each. Shared state size
depends on the caller's Copy error type. The mail error and short per-turn
admission adapter add no heap, retained cursor, source visit or budget charge.
Existing compositions and allocation intervals now exercise the shared
implementation; worker/native qualification remains unchanged and open.

The resident MIME field syntax owner (M06bo) fits 512 bytes in the existing
16 KiB parser reservation, including optional inline CFWS/quoted-string
children and original-budget borrows. It retains no parameter vector, source
copy or output arena. A turn invokes at most one bounded child or scans at
most 32 token octets: 160 visits, 193 aggregate steps and thirteen job records
are the budgeted ceilings. Plain parsing uses at most 32 records per turn.
No output bytes are charged. Allocation intervals cover long comments,
quotes/tokens, construction, malformed suffixes, nesting and final deadline
refusal. Metadata assembly, part traversal and combined worker/native/RSS
qualification remain open.

M06bq's first-valid MIME metadata Cursor and Budgeted each fit 1 KiB in the
existing 16 KiB parser reservation, including one scanner, one inline field
syntax cursor and three fixed optional Field/Head views. There is no source
copy, parameter vector, output arena or allocation during construction,
selection, defaulting or refusal. Plain turns use at most 256 source visits
and 32 job records; budgeted turns use at most 256 visits, 256 aggregate
steps and sixteen job records. Original job/email budgets and private credit
span every candidate and final admission. Cached access is inert; explicit
fresh deadline refusal retires retained metadata. Allocation intervals cover
long comments/quotes/tokens, malformed-first duplicates, defaults, nesting
and raw-header limits. Value derivation, part traversal and combined
worker/native/RSS qualification remain open.

M06br's shared MIME attribute classifier and thin mail Cursor fit 128 bytes;
Budgeted fits 160 in the existing 16 KiB parser reservation. Complete names
remain borrowed source, and base extents/forms/section indices are fixed
scalar state. No segment vector, copied name, output arena or heap allocation
is added. Plain turns charge at most 32 visits and 32 records. Original-budget
composition charges at most 32 visits, 64 aggregate steps and four job records
per turn, with no output capacity charged. Final EOF admission is active;
cached completion is inert until explicit live deadline refusal retires it.
Allocation intervals cover constructor, long names, malformed suffixes,
index overflow and late refusal. Value assembly, part traversal and combined
worker/native qualification remain open.

M06bs's shared MIME value cursor and thin mail facade fit 160 bytes;
Budgeted fits 192 in the existing 16 KiB parser-state reservation. Validation
reuses one fixed shared delimited owner; projection has scalar positions and
language-tag counters, no backing or segment collection. A shared private
mail Admission adapter binds zero-count fresh checks without spending steps
or prepaid record credit. Ordinary and extended octet events remain parsing
evidence; the owner charges output capacity before retained copying.

Each turn visits at most 160 source bytes and charges 32 lexical records.
Budgeted turns cap aggregate steps at 192 and job records at 12, including
original credit. Validation, logical-fold lookahead and percent-source
rereads are charged. Projection emits at most one octet per turn; labels and
all events remain provisional until complete candidate validation. Constructor,
long drain, quoted/percent bytes and malformed/late refusal are Rust-allocation
probe intervals. These are fixed state/counter-model claims, not complete
worker, native/RSS or source-buffer qualification.

M06bt's shared logical reader retains no source, cursor, work or backing.
A single call invokes the caller's bounded admitted byte callback at most
six times, including failed lookahead/EOF. The returned octet, source
position and escaped flag are fixed parsing evidence. Each caller retains
its original source bounds, EOF charges and sticky failure. All phrase,
comment and MIME value cursor sizes and per-turn visit/step/record bounds
remain unchanged; no new resident owner or reservation is added. Existing
allocation intervals cover all three migrated consumers. This extraction
does not qualify the complete worker, native allocator or RSS.

M06bu's complete parameter-family Cursor fits 1024 bytes and its Budgeted
owner 1056 in the existing 16 KiB parser region. It retains inline lexical
children, counters and source extents without a section vector, copied source
or output backing. One turn polls one child or performs a fixed transition;
plain limits are 160 visits and 33 records. Budgeted limits are 160 visits,
256 aggregate steps and 16 job records, including original private credit.
Per-index whole-field replay is explicitly quadratic and every visit is
charged to the original job/header allowance. Aggregate refusal is fatal,
never ordinary fallback, and exhausted HeaderBudget also blocks unrelated
later projections of that email. The pinned fresh-budget fixture with
`attachment;filename=saved` and 400 `filename*N=x` sections is 5915 bytes
and uses 15,759,691 of 16,000,000 steps (98.5%); 600 sections in 8915 bytes
exhaust the allowance and refuse even a later zero-count charge. This is a
fixture cost, not a universal accepted section count: spelling, unrelated
parameters, prior email work and future drain replay change the threshold.
Constructor, long replay, malformed/fallback and
late sticky refusal are allocation-probe intervals. Ancestor frames do not
multiply this owner. Derived output retention, full-worker composition and
native/RSS qualification remain open.

M06bv's parameter Octets owner fits 1088 bytes and BudgetedOctets 1120 in
the existing parser region. One inline Cursor serves complete validation and
charged replay sequentially; neither a second retained selector nor output
backing is added. Original allowances/private credit persist across the
inline reset. The same 160 visits/33 plain records and 160 visits/256 steps/
16 budgeted job-record turn limits hold. Exact-value replay validates and
projects the selected extent; numbered replay repeats full field lookup for
every index. Its additional charged quadratic cost can exhaust the shared
allowance on a family that selection alone accepts; every later projection
then refuses under that exhausted allowance. The same pinned 400-section
fixture emits six provisional Data octets before consuming all 16,000,000
steps, with 10,006,514 source visits left from 16,777,216. The drain then
refuses and none of that prefix is complete output. These numbers qualify
that exact spelling, not a universal section threshold. Bytes are provisional
until
charged drain completion. Constructor, long replay, rejected/fallback and
empty families, and late refusal are Rust-allocation intervals. Complete
worker stack composition, output retention, native/RSS and traversal remain
open.

M06bw's literal parameter scalar Cursor fits 1280 bytes and its Budgeted
owner 1312 within the existing parser region. Its inline Octets owner, one
charset Decoder, 16-byte-bounded passive Label and one held octet replace
buffers or a retained label/string. Ten exact trusted aliases are shared
with existing slice lookup; feeding charges at most ten record units and
retains no source. Each turn polls one child or performs a fixed phase, at
most one provisional scalar. Original 160 visits/33 plain records and
160 visits/256 aggregate steps/16 budgeted job-record bounds still hold.
Constructor, long conversion, known/unknown/empty labels, malformed/fallback
and late refusal are Rust-allocation intervals. Original allowances and
private credit span selection, replay and charset state; no output backing
or capacity is introduced. Retained copying, display/NFC composition,
complete worker stack, native/RSS and traversal qualification remain open.

M06bx's original-source parameter display Cursor fits 1504 bytes and Budgeted
1536 within the existing parser region. The active inline enum contains either
the literal selection/conversion owner or the private 256-byte-bounded
ordinary decoder; it introduces no box, scalar string or label buffer. A
private handoff after complete selection drops the first active owner before
ordinary display begins. Original job/header work and prepaid credit survive
that handoff; source extents are not manufactured publication authority.
Each poll does one child/fixed phase, emits at most one provisional scalar
and spends at most 225 visits/228 plain records or 225 visits/453 aggregate
steps/29 budgeted job records. The maximal 75-byte word fixture reaches 225
visits and 453 aggregate steps; those are new display turn bounds, not the
literal scalar producer's smaller ones. Rust-allocation intervals cover
construction, long Unicode/labels, maximal words, escaped placement, ordinary
and extended malformed/fallback/empty values, Name and late refusal. Retained
output/capacity, NFC checkpoint composition, complete worker stack, native/RSS,
selected boundary and MIME traversal qualification remain open.

M06by's stateless shared character reader holds only four local octets and
scalar positions; it retains no source, cursor or work owner. At most four
atoms make at most 24 read callback calls, followed by one admitted local
verification of at most four bytes. These are helper callback ceilings,
not new parser turn budgets. The migrated readers preserve their exact
source/EOF and verification charges, inline cursor state and all existing
turn/reservation limits. Caller-owned context is borrowed sequentially
rather than cloned. The centralized noncharacter predicate is table-free
and allocation-free; control filtering, diagnostics and retained-output
charging remain with each caller. Existing phrase/comment/parameter and
body/header allocation intervals qualify the migrated paths, not complete
worker/native/RSS bounds.

M06bz moves deterministic ordering/composition into shared td-nfc without
changing mail's resource contract. Scratch remains exactly 3072 bytes, Source
at most 256 and Cursor plus HeaderBudget at most 1024. The shared engine holds
four pure source checkpoints and the exclusive scratch borrow; mail retains
live Meter/HeaderBudget, credit and supplied Tick in its enclosing owner/context.
Generic shared cursor size depends on Source/Error and is not a universal
mail-size claim. One/32-transition source quanta and every scan/insertion/
replay/composition/emission charge retain their existing counts. Existing
Unicode, adversarial checkpoint/prefix, budget and allocation fixtures qualify
the migrated consumer. Larger parameter sources must use their own qualified
parser reservation rather than widening the fixed header Source enum.
Output capacity, complete worker/native/RSS and MIME traversal remain open.

M06ca qualifies a separate parameter normalization Source <=1088 bytes and
Cursor plus HeaderBudget <=4608. Four pure checkpoints live in the engine;
exact nested grammar/decoder/held-octet/decomposition state is copied without
allowances, credit, clock or output backing. Cursor plus two temporary Source
copies, reconstructed display cursor, live context and HeaderBudget fit the
existing 16 KiB parser reservation. NFC scratch stays exclusively borrowed
from the same 3072-byte conversion reservation. Existing header Source <=256
and Cursor plus HeaderBudget <=1024 stay fixed; neither reservation grows.

One admitted normalization transition invokes at most one bounded display
source turn or pending decomposition lookup. Conservative composed ceilings
are 225 original-byte visits, 457 aggregate steps and 30 job records, including
engine/source/decomposition/class admission. Checkpoint restoration does not
reparse normalized completed prefixes or copy/reset prepaid credit. Exact
work/event replay, aggregate/job/output/deadline cuts, long combining replay
and a dedicated Rust allocation interval qualify this source composition.
State-size overlap sums are layout evidence, not native compiler stack or
complete worker/RSS qualification. Retained output remains the later owner.

M06cb qualifies the retained filename owner plus HeaderBudget within 4800
bytes of the existing 16 KiB parser reservation. It owns one parameter NFC
cursor at a time, preserves M06ca's source/temporary bounds and borrows the
same exclusive 3072-byte conversion scratch. One newly active field transition
costs one aggregate step; source turns retain the 225-visit/457-step/30-record
ceilings. Each emitted scalar adds bounded UTF-8 output admission/copy. Output
is caller-reserved backing charged to the enclosing response/work reservation;
no decoded intermediate, per-part string or new ledger pool is allocated.
capacity_bound(n) supplies a checked conservative 16*n UTF-8 byte bound for
one raw selected field value: decoding emits at most one scalar per original
field byte, canonical decomposition emits at most four scalars, and each
scalar needs at most four UTF-8 bytes. Filtering/composition cannot enlarge
that bound; replay does not duplicate retained output. This arithmetic bound
does not authorize allocating an arena of that size: callers reserve within
their existing bounded work window and may refuse an oversized valid name.
Capacity failure retires the entire value. Layout evidence does not qualify
native compiler stack, complete worker composition or RSS.

M06cc's protocol parameter cursor plus HeaderBudget fits 1280 bytes in the
existing 16 KiB parser reservation; the shared ASCII validator fits four
bytes. No conversion/NFC scratch is borrowed. Original octet source turns
remain <=160 visits and <=256 aggregate steps. Each returned Data octet
adds one classification step and charset purposes add ten alias-feed steps;
qualifier octets add ten alias-feed steps. Final completion charges twenty
alias-finish steps. Conservative composed per-poll ceilings are 160 source
visits, 276 aggregate steps, eighteen job records and one output byte.
Caller backing belongs to the existing response/work reservation. A 70-byte
boundary window accepts every grammar-valid boundary; invalid length/alphabet
is drained without further retention once sticky, but full source work is
still charged. Charset tokens have no additional grammar length cap, so
callers choose a bounded window. Overflow stops retention but drains complete
original validation: invalid grammar stays Invalid regardless of its bad byte's
position; valid oversized values refuse OutputCapacity. One raw
field byte bounds one retained token byte. No intermediate vector or new
arena is allocated. Layout/counting evidence does not qualify whole worker,
native compiler stack, portable backend or RSS.

M06cd's resident delimiter cursor fits 160 bytes in the existing parser
reservation; the shared pure line matcher fits 24 bytes. It retains
source and boundary views, fixed prefix/suffix state and one pending CR,
never an input-sized line buffer or a descriptor list. One active poll
charges one job record and examines at most 128 body bytes,
conservatively funding two I/O visits per body byte (source plus at most
one selected-boundary comparison). Initial grammar validation examines
at most 70 boundary bytes with one visit each in one separate turn
phase, including a final-byte grammar failure. Every poll's I/O ceiling
is 256 and job-record ceiling is one. Bytewise funding keeps exact cut
accounting and checks the same supplied clock value before each bounded
access. Delayed CR classification spends the prior funded comparison
under a new live turn's original admission. No aggregate header
interpretation allowance is spent on these raw body bytes. This
qualifies resident scanner state/work only, not DFS composition,
complete worker/native stack or RSS. Child extent clipping and
body/descriptor reservations remain those of the later traversal owner.

M06ce's private protocol Reader, delimiter Core and metadata context poll
replace their former inline mechanisms. Public cursor bounds and original
per-turn charging are unchanged. Detached shared delimiter State fits eight
bytes; Line still fits 24 bytes. No extra source/output backing, allowance,
checkpoint replay, parser arena or descriptor reservation is introduced.
Moving private progress preserves state and has no allocation or admission
effect. A future traversal must qualify its simultaneous core/frame layout.

M06cf's resident traversal cursor plus HeaderBudget fits the existing 16 KiB
parser/boundary reservation, including 64 simultaneous explicit frames,
phase-exclusive metadata/parameter progress, one transfer decoder and 32
counting bytes. A Part fits the existing 64-byte descriptor cell; backing
is separate, bounded by configured mime_parts, and supplied by the caller.
Variable metadata remains source extents. Root/child recognized headers
share the configured aggregate header_bytes count, while replay/selection
share the original 16 MiB source-visit/16-million-step HeaderBudget and its
prepaid credit. No stage, output arena or descriptor pool is added.

The measured x86-64 host layout is Cursor 11928 bytes, HeaderBudget 24
bytes and Part 56 bytes: 11952 parser bytes leave 4432 bytes of the existing
16 KiB reservation. The committed const assertion keeps the target-independent
ceiling; these host sizes are not native stack/RSS measurements.

M06di stores digest-child context in a previously unused flag bit, so Part
remains 56 host bytes and traversal layout/output charges are unchanged.
The combined Part, body-list Node and membership byte still fit their
existing 64-byte reservation. The existing combined compile-time assertion
pins that ceiling.
Context is passive evidence independent of selected type, never new source
or response authority. Warm/measured traversal intervals also pin nested
context retention without Rust allocations. A separate unit qualifies
completed traversal-to-retained-metadata handoff under original owners at
zero/nonzero/maximum-fitting absolute bases. Nested digest/mixed containers
also pin immediate-child context and preservation beside after-append
missing-close/ignored-suffix problems. A separate problems() accessor masks
context from the five parse-problem flags. Native/worker/RSS and automatic
whole-response composition remain separate.

One poll advances one phase and funds one structural job record. Conservative
ceilings are 352 I/O/header visits, 512 aggregate interpretation steps,
32 job records and 96 output bytes per turn. Raw delimiter work retains its
separate 128-body-byte turn and original I/O charging. Transfer size counting
uses a 32-byte reusable output window and at most 256 decoder transitions;
retained descriptor copies also fund their actual cell bytes. Identity sizes
come directly from checked extents without an unnecessary byte scan. No
charset or NFC workspace overlaps traversal state.

Parent-first clipping re-visits each container body at every enclosing
container, so total delimiter visits scale with the sum of container body
lengths (bounded by structural depth times encoded size). Each raw 128-byte
turn spends two job records: traversal plus scanner. A nearly 32 MiB leaf
inside 31 containers needs about 16.3 million delimiter records before
metadata/decoding; structural admission alone does not promise completion
under ADMISSION.md's default two-million-record foreground cap. All visits
and records spend that same original job allowance. Deterministic work-cap
refusal is a method failure under ADMISSION.md section 3/response policy,
never a successful partial tree or fabricated notParsable syntax. Raw
preservation and metadata-only recovery remain available. No budget is raised
or renewed here; worker wiring must choose the existing validated job budget.

Size counting also spends original output allowance for discarded decoded
bytes, as the existing decoders require, in addition to descriptor retention.
This dimension measures emitted/staged work rather than wire response bytes
alone. Foreground integration must account for it without a fresh meter.

Tests pin full depth 64 and 4096 cells, aggregate entity-header and email
interpretation cuts, original work cuts, fresh deadlines and passive handoff.
Rust allocation intervals cover resident nested/digest trees, empty and
missing-close parts, long QP replay, malformed transfer and source syntax,
work refusal and late handoff. This is resident Rust state/allocation evidence,
not a native compiler stack, portable provider, whole worker or RSS claim.

M06cg's retained part-header cursor plus HeaderBudget fits the existing
16 KiB parser reservation. It runs after traversal releases that
reservation; its phase-exclusive child owns the original job/header
budgets and borrows one original 3072-byte NFC Scratch in the existing
conversion partition. Complete selected fields and token ranges borrow
resident source; no copied field string, input-sized state or additional
descriptor is retained.

M06cv adds two selected passive raw label extents and one exclusive
mime_label_fields child to this same cursor reservation. The child replaces
other live projection children, borrows the original Meter/HeaderBudget and
leaves original Scratch untouched. A second raw scan uses original source
and interpretation work without counting raw headers twice; matching End
metadata pins this correspondence. Existing caller windows remain unchanged.
Allocation intervals include malformed-then-valid CID/language fields and
four-byte comments alongside retained heads/filename, plus late needed-label
nesting refusal. No CID string or tag list, additional descriptor arena,
whole-worker or native/RSS bound follows.

Backing's heads, charset and filename are separate caller-reserved
windows in existing admitted output/text backing. Head capacity needs
the complete type and optional disposition token bytes (including the
one canonical slash); heads_capacity_bound adds a checked fourteen-byte
default floor to recognized raw entity-header bytes, so empty/digest
defaults remain representable. Charset capacity follows selected Data
bytes; filename capacity can use the existing checked
mime_filename::capacity_bound of the larger selected disposition/type
field-value byte length. Capacity shortfall fails the entire projection;
windows never grow. Retaining many Views requires the caller's aggregate
output reservation, not a per-part renewed allowance.

Head copying funds one interpretation step for each bounded transition,
including absent slots, plus one step per copied byte (at most 33 steps
in a head-copy turn). It processes at most 32 bytes per turn and charges
source visits, interpretation steps and output before each copy. Other
turns use the existing metadata, protocol and filename child bounds; M06cv
label turns also inherit M06cu's 256 source visits, 256 interpretation steps
and 16 job records per turn; phase transitions do no input-sized work. Public child handoffs abandon
unused prepaid credit without refunding any counters; they never
recreate job/header budgets or scratch. Raw headers reported by replay
are not counted again as new structural bytes. Literal slash/default
bytes are conservatively charged as visits too; this bounded overcharge
avoids a separate generated-byte funding path. Every visit, lowercasing
and emitted retained byte spends original admission.

Tests cover first-valid/default/digest selection, casing, selected-empty
name, invalid/absent/unknown charset, long tokens, all five allowance
cuts, every live deadline turn, all three window capacities and fresh
original handoff. The dedicated warm/measured Rust probe covers
completion, defaults and sticky capacity retirement. No native compiler
stack, whole worker or RSS is claimed.

M06ch's body-list cursor plus HeaderBudget fits the existing 16 KiB parser
reservation and runs after traversal and selected-header projection release
that state. Its fixed 65 frames include a virtual mixed scope and 64 entity
levels. There is no recursive production call or per-container heap list.

Part plus Node plus one membership byte fits the existing 64-byte per-part
reservation, enforced together at compile time. Node carries only parent,
depth and compact Class; it is supplied separately from the retained
traversal Part. Class comparison conservatively funds 64 header visits/steps
and its actual emitted cell bytes through the original owners. It assumes
canonical retained metadata, not an independent source or authorization.

At most three u16 ordinal lists with 4096 entries each require 24 KiB of
separately admitted caller aggregate output backing. The 96 KiB body-work
reservation is fully partitioned and supplies no retained list storage.
Lists outlive the cursor and may coexist with subsequent
decoding/conversion; automatic worker coordination must reserve that
simultaneous retention before use. Fixed frames remain in parser state.
Backing is never enlarged. Body/list work retains the original HeaderBudget
exclusively and freshly checks it without changing its byte/step counters.
Each poll funds one structural job record, at most eight
node/membership/fallback I/O bytes and seven emitted bytes. One fallback
entry is copied per turn; no whole-list copy or repeated source-prefix scan
occurs. Membership is initialized as each node validates, with no unbudgeted
refusal wipe. Final attachment classification scans one node per turn after
fallback.

Tests pin the RFC's literal A..K lists, sole-alternative fallback, related
and named/empty behavior, inherited disabled channels, final attachment
membership, invalid preorder, full depth/parts, all output windows, every
job allowance cut, sticky header refusal and every live deadline turn. Rust
warm/measured allocation intervals cover simple/nested/deep success,
malformed tree, work and capacity failure and late retirement. State/cell
ceilings are independent of native compiler stack, complete worker, portable
provider or RSS evidence.

M06cj's Content-Language cursor plus HeaderBudget fits 256 bytes in the
existing 16 KiB parser reservation. Its shared lexical cursor with mail's
fixed error fits 192 bytes; CFWS and passive Tag are inline, with no recursive call or
per-tag heap storage. Each poll visits at most 160 source bytes and spends at
most 192 header steps and 12 original job records, with private retained
credit. EOF and repeated lookahead are charged; no output bytes are spent by
passive raw extent events. Enclosing retention/publication must fund any
stored descriptors or projected strings separately.

Shared literal RFC examples and complete-value refusal fixtures qualify the
syntax, including long tags/comments and every callback cut. Mail fixtures
pin original owner identity, exact aggregate accounting, all header/job cuts,
every live deadline turn and fresh completion refusal. A long four-byte UTF-8
comment reaches exactly 160 visits, 192 header steps and 12 job records in a
turn. Warm/measured Rust allocation intervals cover success, syntax and
nesting refusal, job-record exhaustion and deadline retirement after
completion. HeaderBudget refusal and mid-parse deadline cuts have unit
coverage; worker/native/RSS remain unqualified.

M06ck's Content-ID adapter plus HeaderBudget fits 512 bytes in the existing
16 KiB parser reservation. It wraps the existing budgeted identifier
converter; no second parser, heap list, output backing or NFC scratch is
created. Private purpose tags change grammar placement, not state capacity.
Each poll stays within 160 source/intermediate visits, 255 header steps,
sixteen job records and four output bytes. Both intermediate unfolding bytes
and scalar UTF-8 bytes are charged; retained output admission is separate.
Syntax validates the full single-identifier value before any projection event.

Literal mail fixtures pin projection, original owner identity and aggregate
accounting, multiple-identifier rejection, malformed/nested tails, all
header/job/output allowance cuts, every live deadline turn and fresh handoff.
Warm/measured Rust allocation intervals cover long Unicode success, repaired
noncharacters, multiple-identifier/nesting refusal, job-record/output refusal
and original handoff. Header cuts and live/late deadlines have unit coverage;
worker/native/RSS remain unqualified.

M06cl's shared URI validator with mail's error fits 128 bytes and requires a
scheme. It replaces the prior private validator inside URL state in the
existing 16 KiB parser reservation. Original URL and budgeted-state ceilings
and per-turn work remain unchanged. Input octets are admitted by the same
outer mail grammar; IPv6 closure funds 64 records before a <=45-byte local
std parse. No copied source or renewed budget is introduced. The shared feed
API's 64-record parse is separate from resident lexical poll maxima. Shared
literal URI, malformed authority/percent tail and sticky callback refusal
fixtures qualify generic syntax. Existing full wrapped mail suites pin exact
two-pass/IPv6 costs, bounded turns and warm/measured Rust allocations.
Relative values and worker/native/RSS remain unqualified.

M06cm's URI-reference mode adds fixed prefix/scheme-disambiguation state
inside the same 128-byte concrete shared validator ceiling. Default mail
URL consumers retain their constructor and original source/job/header work.
Relative and empty references use the same bounded feed and optional
64-record <=45-byte IPv6 parse; no prefix buffer, heap list, decoded URL,
base resolver or growing state is introduced. Shared literal RFC references,
first-segment colon/percent tails, authority failures and sticky live/final
refusal qualify syntax. Existing wrapped mail tests qualify default-mode
compatibility. Warm/measured Rust allocation intervals exercise relative,
empty, long, IPv6/IPvFuture, malformed, prepaid-work and fresh-final refusal
paths with fixed caller work; they do not qualify mail field admission.
Content-Location integration and worker/native/RSS remain open.

M06cn's shared URI unfolding cursor fits 64 bytes with the public mail URL
error type and owns no growing storage or output collection. Each poll
admits one source octet or EOF before access: at most one visit and one
record. Runs of whitespace remain bounded turns. Offsets within the supplied
spelling slice and literal octets remain provisional; complete unfolding
cannot replace enclosing admission. Cached completion is inert and fresh
zero-count refusal retires it. Shared fixtures pin exact source/EOF costs
and every callback cut. Warm/measured Rust allocation intervals cover empty,
relative, long, folded encoded-word spelling, malformed folds,
first-read/EOF refusal and late fresh refusal with a fixed generic callback;
they do not qualify mail field/source admission, encoded-word composition or
worker/native/RSS.

Shared td-json framing plus its private mail adapter fits 32 bytes and owns no
source or work reference. This is the bound with the mail error enum; generic
Frame<E> size also depends on E. The public borrowed adapter still fits 64
bytes; extraction does not introduce another simultaneous frame. A future
coordinator may retain its parser and frame in separate fields and form short
source borrows per poll. NFC's external workspace and existing reservation
stay separate. Allocation intervals through the public adapter cover the same
extracted framing actions; a movable test owner validates live-meter
borrowing, not a complete worker memory bound.

The parsed/fallback address text facade fits 416 bytes in the body parser
reservation. Private purposes reuse the MessageIds conversion engine,
including the fixed one-byte handoff; public MessageIds state remains
within 384 bytes. Fallback edge trimming uses scalar offsets, and charset
replacement retains no malformed byte string. No new arena is allocated.
Intermediate unfolding bytes and final UTF-8 scalar bytes both charge the
same output budget. Allocation intervals cover long Unicode, malformed
UTF-8, noncharacters, literal controls, empty values and refusal. Complete
display-name/NFC, response storage and worker stack composition remain open.

The budgeted Date wrapper (M06av) fits 224 bytes, including the inline cursor,
budget references and private credit, in the existing 16 KiB parser region.
It replaces the standalone Date cursor on this path. Each poll charges at
most 161 source visits, 194 aggregate steps and 13 job records, without output
bytes. Child CFWS turns share its budget and credit. Allocation intervals
cover long Unicode comments, malformed tails, shared budgets and retirement;
combined worker/native/RSS qualification remains open.

The date-time cursor fits 192 bytes in the same body parser reservation,
including its CFWS cursor, fixed token prefix and calendar components.
Comments, arbitrarily zero-prefixed years and unknown zone names retain no
copied strings. Isolated allocation intervals cover successful long inputs,
malformed dates/tails and nesting refusal. No time-zone database or native
calendar call is used. Ordinary date projection formats checked components
into 20 or 25 caller bytes. Placement and exclusive lifetime of that output
remain the enclosing owner's responsibility; no existing scratch partition
is claimed by this helper. Its calendar shift is bounded to five days and
uses scalar locals. It allocates no time-zone table or owned string. The
standalone formatter retains its caller-output obligation; M06ax supplies
the complete Date property reservation below. Combined worker stacks remain
open. Pinned leap qualification adds only the static payload above. M06aw's
budgeted formatter adds a transient adapter of at most 24 bytes, with the
same 25-byte caller output obligation. It charges at most 14 aggregate steps,
one prepaid job record and 25 output bytes, without source visits. Allocation
intervals cover ordinary/leap formatting, invalid/unverified outcomes and
refusal. These component bounds do not qualify the combined worker stack.

The Date property coordinator (M06ax) fits 896 bytes inside the existing
16 KiB parser reservation, including inline Date/CFWS state, its budget
references, 27 bytes of staged JSON and shared selection/framing state.
It replaces standalone Date parsing/output storage on this path, borrows no
NFC scratch and allocates no arena. The formatter's transient 24-byte adapter
is unchanged. Turn bounds remain 255 visits, 256 aggregate steps, 16 job
records and six external output bytes. Allocation intervals cover long
comments, multiple dates, leap outcomes and late selection refusal. Combined
worker/native/RSS qualification remains open.

The budgeted MessageIds grammar (M06ay) fits 288 bytes, including inline
CFWS/delimited state, original budget references and private prepaid credit.
It replaces its standalone grammar cursor in the existing 16 KiB parser
reservation. The transient shared Parsing adapter consists of three
references; no scratch region or process allowance grows. A turn charges at
most 160 visits, 192 aggregate steps, 12 job records and zero output bytes.
Allocation intervals cover long Unicode comments, atoms and delimited tokens,
multiple fields, malformed tails and terminal refusal. M06az adds scalar
conversion below; property JSON and combined worker/native/RSS qualification
remain open.

The budgeted MessageIds converter (M06az) fits 416 bytes, replacing the
standalone converter in the existing 16 KiB parser reservation. It includes
inline parser/unfolder/charset state, one conversion byte and original budget
references; no NFC scratch or owned string/list is needed. Its transient
adapter holds three references. Private unfolding turns use 127 transitions;
public plain turns retain 256. A conversion poll stays within 160 source or
intermediate visits, 255 aggregate steps, sixteen job records and four output
bytes. Probe intervals cover long text, folds, repairs and refusals. M06ba
adds property JSON below; combined worker/native/RSS qualification stays open.

The MessageIds property coordinator (M06ba) fits 1024 bytes inside the
existing 16 KiB parser reservation, including inline conversion, shared
selection/JSON framing and literal buffers. It replaces standalone conversion
state and uses no NFC scratch or arena. Mode selection reads at most eleven
compared bytes in its own charged turn. Overall turn bounds remain 255
source/intermediate visits, 256 aggregate steps, sixteen job records and six
external output bytes. Combined conversion/JSON output charges are at most
eight bytes per poll. Probe intervals cover long arrays, malformed fields,
repaired scalars, one-byte drains and late refusal. Combined worker/native/RSS
qualification remains open.

The budgeted URLs cursor (M06bb) fits 288 bytes in the existing 16 KiB parser
reservation, replacing standalone URL state. It retains CFWS/URI state, the
45-byte IPv6 array and original budgets; a shared transient Conversion adapter
uses three references. No scratch or process allowance grows. A turn charges
at most 160 visits, 193 aggregate steps, thirteen job records and one output
byte. Probe intervals cover long comments/URLs, literals, NO and refusals.
M06bc adds URL property JSON below; combined worker/native/RSS qualification
remains open.

The URLs property coordinator (M06bc) fits 1024 bytes in the existing 16 KiB
parser reservation, replacing standalone URL state with shared inline list
framing. It includes selector, URI/CFWS state, live budgets, frame and literal
buffers; no NFC scratch, owned URL/list or arena is needed. Mode selection
compares at most nine charged bytes. Turn ceilings remain 255 visits, 256
steps, sixteen job records and six externally copied bytes. Combined URL/JSON
output charge is at most four bytes per poll; the shared MessageIds path
retains its eight-byte charge ceiling. Allocation intervals cover long URLs,
null/NO, short drains and late refusal. Combined worker/native/RSS
qualification remains open.

M06cx replaces mail's inline array punctuation/string loop with shared
std-only td-json framing. Each existing MessageIds/URLs coordinator still
fits its 1024-byte parser ceiling with original owners, one pending event,
fixed null literal and the shared array frame. The shared frame owns at most
one six-byte string pending buffer and fixed phase/error state; it invokes
one source poll, one bounded string turn or copies one punctuation byte per
call. Existing source/step/record/output ceilings above remain unchanged.
Whole-field validation precedes array output; malformed null mapping adds no
fresh allowance. Existing measured allocation intervals exercise the shared
array implementation without a retained list or arena. Standalone frame
size depends on the caller's inline error type; no universal error-size
bound or native/worker/RSS qualification follows.

The budgeted address/group parser (M06bd) fits 800 bytes in the existing
16 KiB parser reservation, replacing standalone list state. Original budgets
and private credit cover boundary/CFWS/phrase/mailbox/route parsing and raw
recovery. No token text or list is retained. Turn ceilings are 161 source
visits, 196 aggregate steps, thirteen job records and zero output bytes.
Allocation intervals cover long group/name input, routes, comments, malformed
recovery and terminal refusal. Budgeted text/NFC composition and combined
worker/native/RSS qualification remain open.

The budgeted address text facade (M06be) fits 448 bytes in the existing
16 KiB parser reservation, replacing standalone text state. It reuses the
budgeted conversion engine, original budgets, private credit, one-byte handoff
and 127-transition unfolding turn. No NFC scratch or new arena is needed.
Per-turn ceilings are 160 visits, 255 aggregate steps, sixteen job records and
four output bytes. Allocation intervals cover long Parsed/Fallback text,
repair, noncharacters and refusals. Name/property composition and combined
worker/native/RSS qualification remain open.

The selected display-name coordinator (M06bf) fits 1280 bytes in the existing
16 KiB parser reservation and borrows the existing 3072-byte NFC scratch.
Its inline normalization state replaces the NFC region's standalone cursor;
that cursor slot is unused on this path. Validation and normalization run
in separate turns with the original budgets and separate private credit.
Per-turn ceilings are 255 visits, 256 steps and sixteen job records; scalar
events carry no serialized-output charge. No name/token list or extra arena
is retained. Allocation intervals cover long phrase/comment input, overflow
replay, scratch reuse and final refusal. JSON composition and combined
worker/native/RSS qualification remain open.

Budgeted address/name JSON adapters (M06bg) retain the existing 64-byte
borrowed coordinator and 32-byte frame. Address/name owners keep their prior
parser/scratch reservations; no additional string, collection or arena is
retained. Turn ceilings remain 255 visits, 256 steps, sixteen job records
and six externally copied bytes; address sources retain their tighter
160-visit/255-step ceiling. Combined conversion/JSON output charges are at
most eight bytes for addresses and six for names; framing adds no
interpretation steps. Allocation intervals cover long inputs, original
budgets/scratch, one-byte drains and output refusal. Complete property
assembly and worker/native/RSS qualification remain open.

The Addresses property owner (M06bh) fits 2560 bytes of the existing 16 KiB
parser reservation, including selection, suspended address grammar, active
conversion, frame and nine-byte literal staging. It borrows the existing
3072-byte NFC scratch and replaces that region's standalone NFC cursor with
its inline normalization state. It retains one mailbox extent, not a list of
names, addresses or groups. Original job/email budgets and the suspended
parser's previously funded credit survive each checked child handoff.
Per-turn ceilings are 255 visits, 256 steps, sixteen job records, nine charged
output bytes and six externally copied bytes. Allocation intervals cover long
normalized names, identity addresses, raw recovery, multiple fields, scratch
reuse and late selection refusal. Grouped values, unpublished retention and
combined worker/native/RSS qualification remain open.

GroupedAddresses (M06bi) shares the static Addresses coordinator, retaining
the current group name extent, group/comma flags and name-continuation phase.
It fits the same 2560-byte parser reservation and borrows the same 3072-byte
NFC scratch; group and mailbox names reuse one inline normalization owner in
turn. The fourteen-byte group addresses key/opening-array literal is split
across two staging turns, preserving the nine-byte charge/staging ceiling and
all other Addresses turn bounds. No group or member list grows. Allocation
intervals cover long group/mailbox names, identities, multiple fields, scratch
reuse and late selection refusal. Complete worker/native/RSS qualification and
response-spool publication remain open.

The unified header-value dispatcher (M06bl) retains exactly one selected
coordinator in a private inline enum, within 2560 bytes including the tag.
It replaces that selected coordinator's standalone state in the existing
16 KiB parser region and borrows the same 3072-byte NFC scratch. No second
parser, box, source copy or extra reservation is introduced. Its forwarding
adds no work charges or input scans, preserving the coordinator ceilings.
Allocation intervals include dynamic construction, one-byte drains and refusal
for all seven forms, plus normalization overflow for Text, Addresses and
GroupedAddresses. These component checks leave combined worker stacks, native
allocations and whole-process RSS unqualified.

The charset decoder uses at most 32 bytes of copied state, fitting the 32
KiB conversion region's 2 KiB decoder/HTML/snippet state. A saved copy also
fits within UNICODE.md's future 256-byte decoding cursor checkpoint. No
partition or process allowance grows. One turn inspects at most four source
bytes and returns at most one scalar, with no owned string or arena. An
isolated Rust allocation interval covers all four charsets, single-byte
fragments, malformed UTF-8, replacement diagnostics, completion and sticky
work refusal. No complete header normalization pipeline or worker-stack
bound is claimed.

Header unfolding uses at most 16 bytes of copied state, fitting the same
2 KiB decoder/HTML/snippet state and a future decoding cursor checkpoint. At
most two output octets await caller capacity; no line or field buffer grows.
The isolated allocation interval covers single-byte source/output fragments,
fold/nonfold endings, stable completion and fresh-meter refusal retries with
unchanged Rust counters. Full header-form composition remains unqualified.

The resident Raw header cursor fits 64 bytes including its immutable source
reference and UTF-8 decoder. It fits the same 2 KiB decoder/HTML/snippet
state and a future 256-byte decoding cursor checkpoint; no retained
header string is allocated. A dedicated interval covers raw scalar decoding,
NUL removal, malformed/noncharacter replacement, replay, completion and
fresh-meter refusal without changing Rust allocation counters. Source
collection and the complete projection worker stack remain unqualified.

The encoded-word payload cursor fits 128 bytes including its borrowed Word,
three pending transfer bytes and charset state. It uses the same decoder
region and a future 256-byte cursor checkpoint. One poll visits at most four
payload bytes and one charset byte and emits at most one scalar. Isolated
Rust allocation intervals cover Q/Base64 success, malformed-unit recovery,
control/noncharacter filtering and copied replay without counter changes.
No complete header pipeline or worker-stack bound is claimed.

M06co's private payload-free encoded-word Progress fits 48 bytes. On the
64-bit host it is 40 bytes; the public wrapper grows from 72 to 80 bytes
while remaining within 128 bytes and existing enclosing cursor ceilings.
The usize payload length preserves the const constructor and comes only
from recognition's at-most-75-octet Word, matching u8 transfer positions.
The same decoder region and checkpoint ceilings apply; no scratch buffer or
second header arena is added here. Each admitted turn checks charset,
encoding and length before payload access, within the existing first record
charge. Public Q/Base64 repair, charset/control/noncharacter filtering and
exact visits/records remain unchanged. Relocated equal-word fixtures pin
scalar, diagnostic and work correspondence, not byte identity or enclosing
source admission. Existing warm/measured allocation intervals exercise the
public wrapper's migrated core; they do not qualify a future scratch-backed
Content-Location pipeline or complete worker/native/RSS bounds.

M06cp's selected URI word cursor plus HeaderBudget fits 512 bytes in the
existing parser region. Its only payload scratch is a fixed 75-octet array;
private relative Descriptor fits 16 bytes and the actual mail-error URI
unfolder fits 64 bytes. No second header arena is introduced. Each poll
visits at most 225 source/scratch bytes, spends at most 452 interpretation
steps and 29 job records, and emits at most one scalar with up to four UTF-8
output bytes. Recognition prepays bounded scans once; reconstruction reads
relative metadata without rescanning bytes. Oversized candidates retain no
growing buffer and continue complete wire validation before literal fallback.
The selected-reader allocation intervals cover folded Q/Base64 success,
repair, unknown/oversized fallback and late malformed/fresh deadline refusal.
Whole-field placement, pipeline/worker/native/RSS qualification remain open.

M06db's URI encoded-word run reader retains the existing 512-byte ceiling for
Cursor plus HeaderBudget and one 75-octet scratch buffer. Shared passive
framing fits four bytes and owns no work or source. Complete fold validation,
whole-run classification and decoding replay use three separately funded wire
passes; classification and replay each recognize every healthy bounded word.
All passes use the same original allowances and prepaid credit. No word list,
second header arena or source-sized scratch is introduced. Each poll retains
the 225-visit, 452-step, 29-job-record and four-output-byte ceilings, with at
most one scalar. Unknown or mixed runs emit no decoded prefix; later resource
refusal retires provisional output.

Warm/measured Rust allocation intervals cover a 2048-word folded run, exact
75-octet words, combined diagnostics, absence, unknown/mixed/oversized fallback,
late malformed folds, zero records and late scalar output exhaustion. They
also qualify original owner reuse and fresh completed retirement. Input
allocation precedes the measured interval; worker/native/RSS and complete
Content-Location placement/retention remain separate qualifications.

M06dc's complete authorized URI field cursor plus HeaderBudget fits 640
bytes in the existing parser reservation. It retains one active fixed
selector, word-run reader or literal reader, never simultaneous live work
owners. Word mode keeps one 75-octet buffer; literal fallback allocates no
source-sized storage. Each poll invokes at most one child within the existing
225-visit, 452-step, 29-job-record and four-output-byte ceilings, emitting at
most one scalar. Zero-charge fresh consuming handoffs forfeit unused credit
without renewing it; all scans, replay and output use original allowances.

Units compare output, diagnostics, poll counts and all original job/header
balances with independent public selection/word/literal pipelines, including
every resource cut and exact grants on both forms and unknown fallback. Equal
turn counts pin one child poll per composer turn; the numeric ceilings are
inherited from independently qualified child turns. Every-prefix fresh
and premature admission, metadata retirement and original owner reuse are
qualified. Warm/measured Rust allocation covers long literal and word runs,
CFWS, diagnostics, empty/fallback, malformed folds/URI/CFWS, zero records and
late output refusal in either projection form. All input allocation precedes
counting; worker/native/RSS and retained response buffers remain separate.

M06dd's location JSON cursor plus HeaderBudget fits a compiled 768-byte
ceiling in the existing parser region. It adds the shared fixed six-byte
framing buffer to the complete field reader, with no growing output or
second header arena. Each turn invokes at most one field poll or copies at
most six prepaid serialized bytes. Inherited child ceilings remain 225
visits, 452 interpretation steps and 29 job records; a scalar turn may spend
four decoded bytes plus six serialized bytes. Framing spends only exact
job output bytes after live original header admission, never new credit.

Two units compare shared-writer JSON and original costs with independently
projected field scalars, including exact grants and all resource cuts. They
pin identical visits, records and header steps plus exact serialized output;
non-output cuts retain the independently observed scalar phase/context.
Healthy per-turn deltas pin inherited visit/step/record and combined output
ceilings; a supplementary scalar requires four decoded plus four serialized
bytes, split across short drains. Failed-turn ceilings remain qualified by
the child readers. The generic six-byte wire ceiling stays conservative under
the URI source's inherited encoded-control dropping. Widths one through eight, empty interleaves,
every original resource cut/exact grant, every-prefix deadlines/premature
finish, malformed fields, typed framing refusals and owner reuse are covered.
Warm/measured Rust allocation qualifies long literal and folded word fields,
short escape drains, inherited control dropping, repair/absence/fallback,
malformed tails, zero records/output and late scalar/closing refusal. Inputs
are allocated before counting; worker/native/RSS and retained response
buffers remain separate qualifications.

M06df's resident location collector plus HeaderBudget fits a compiled
1024-byte ceiling within the existing parser reservation. One scanner and
one exclusive inline field child use fixed state; selected raw extents and
diagnostics allocate no retained output. Live turns freshly admit original
owners and perform at most one scanner/field poll. Healthy fixtures check
256 job visits, 452 interpretation steps, 29 job records and four scalar
output bytes per turn, with identical job/header source visits. Failed-turn
bounds remain those of the independently qualified scanner/field children.
Discarding provisional scalars adds no copying, but preserves their original
output charges. Discovery followed by retention must fund the selected
field's scalar output twice, plus the retained JSON wire output; malformed
needed occurrences also spend their original interpretation/output costs.
Malformed handoff adds no charge or renewed allowance;
remaining funded child credit is discarded, never reset to a positive grant.

Five units qualify first-valid/empty/repair selection, original raw-source
retention and reusable owners, every progress-prefix admission/premature
finish, typed malformed-only discard and resource/nesting/structural refusal.
An independent raw scanner plus public field projection derives child costs;
the reference separately funds the specified name comparison and final
bookkeeping charges. Malformed-owner snapshots pin zero-cost handoff, and
every positive original resource cut and exact grant checks whole visibility.
Warm/measured Rust intervals cover long literal/word fields, 1024 malformed
needed and ignored later duplicates, repair/empty/absence, complete body
isolation, prefix/header bounds and zero-record/output/nesting refusal.
Inputs and optional retained backing allocate before counting; completed
original-owner reuse and fresh whole-view retirement are measured. Worker,
native, RSS and response-backing reservations remain separate qualifications.

M06de's selected location retention cursor plus HeaderBudget fits a compiled
1024-byte ceiling in the existing parser region. The shared window owns one
fixed reserved backing and checked prefix/failure state within four machine
words; retention buffers remain separate caller-reserved response storage.
CID/language retention uses two such windows without increasing its existing
1024-byte cursor ceiling or the composed part-header 6 KiB ceiling. The two
windows add two inline refusal slots: the paired cursor grows from 352 to
368 bytes on the 64-bit host. M06dh qualifies the current composed cursor,
whose larger header owner still dominates. Existing owner ceilings still
compile. No growing storage, source-sized scratch or second parser/header
arena is introduced.

A location turn performs fresh original admission and at most one JSON child
poll; a completing poll freshly consumes/restores original owners in that
turn without added resource charges or credit. Child visit/step/record and
ten-output-byte ceilings remain inherited. Window prefix bookkeeping copies
no bytes and grants no output. Missing input consumes no window or output;
capacity/projection/deadline failures retire the entire result. Shared checked
string sizing retains the existing CID 6N+2 bound and supplies the location
bound; transforming-source policies justify both raw-octet bounds separately.

Units compare retained values, diagnostics, backing and original-owner
identity with independent public JSON projection and exact original costs.
Every backing capacity and every original resource cut/exact grant, every
prefix deadline/premature finish, absence versus empty presence, malformed
fields and relocation between every turn qualify whole visibility. Existing
paired-label resource/allocation oracles qualify the window migration.
Warm/measured Rust intervals cover long literal/word values, exact backing,
short escapes, repair/absence/fallback, syntax/zero/late output refusal,
empty/full capacity, original public reuse and fresh completed retirement.
Input/backing allocation precedes counting; native, worker/RSS and enclosing
response retention reservations remain separate qualifications.

M06dg's source-bound location collector/retainer plus HeaderBudget fits a
compiled 1536-byte ceiling in the existing parser reservation. One inline
selector or retained child owns the original mutable allowances. One
immutable resident Input and one reserved backing remain bound throughout;
no source-sized scratch, growing collection or second header arena exists.
Handoff grants no allowance, performs no extra copying and polls no second
child. Child charges remain unchanged: discovery funds discarded scalars,
retention funds replayed scalars and JSON wire output separately. A turn is
bounded by the larger child ceiling: 256 job/header visits, 452 interpretation
steps, 29 job records and ten scalar-plus-JSON output bytes. Reconstructed
original-owner snapshots pin those limits at every turn without a new
counter-access surface.

Five units compare public selector-plus-retainer output, diagnostics, exact
original costs and pointer-identical reusable owners; sweep all five
positive original resource costs and every output capacity; pin combined
poll counts against separately run public children; and qualify
fresh checks/finish at every progress prefix and late scanner refusal before
retention. Warm/measured Rust intervals cover long literal and folded-word
fields, 1024 malformed needed occurrences, empty/repair/absence and capacity
refusal, original-owner reuse and fresh whole-view retirement. Source and
backing allocate before counting; native, RSS, worker and response storage
remain separate qualifications.

M06cq's selected literal URI reader plus HeaderBudget fits 320 bytes in the
existing parser region. It retains shared fixed validator/unfolder state
and source borrows, with no source-sized scratch or second header arena.
One poll visits at most one source byte, spends at most 66 interpretation
steps and five job records, and emits at most one charged ASCII output byte.
The bounded IPv6 parse prepays 64 lexical records through those owners,
mapped to 64 interpretation steps and four job records. Complete validation precedes projection; funded replay
preserves slice-relative offsets and exact literal spelling. Warm/measured
Rust allocation intervals include empty, long, folded, IPv6/IPvFuture,
word-marker literal, malformed/refused and fresh final handoff paths.
Whole-field and worker/native/RSS qualification remain open.

M06cr's shared URI spelling selector fits 160 bytes with u8 or mail's public
literal-reader error, retaining only source offsets and exclusive CFWS or
unfolder child state. Generic layout depends on E; this is not a future
composer-size claim. One poll stays within 160 visits and 32 lexical records.
All whitespace probes, optional suffix retries and CFWS replay are funded;
no total linear-work guarantee follows. Warm/measured Rust allocation
intervals cover long whitespace, comments, partial/malformed suffixes,
leading syntax/nesting errors, callback refusal and fresh final retirement.
No new arena or source-sized scratch is reserved. Original owner integration
and worker/native/RSS qualification remain open.

M06cs's mail URI spelling selector plus HeaderBudget fits 256 bytes in the
existing parser region; its actual private-error shared state fits 160 bytes.
A poll visits at most 160 source bytes, spends at most 192 interpretation
steps and 12 job records, with no conversion output. All probes and replay
use original allowances and prepaid credit. Consuming handoff freshly admits
and returns those same owners without new grants. Up to 15 unused prepaid
steps are discarded at handoff; the next phase may need one additional job
record compared with a ceiling over combined step counts. Allocation intervals cover
long whitespace/comments, optional-tail recovery, syntax/nesting/job refusal,
healthy handoff and late retirement. No extra arena or selected-value copy
is allocated. Whole-field projection and worker/native/RSS remain open.

M06ct's literal field-value coordinator plus HeaderBudget fits 384 bytes
within the existing parser region. Exclusive phase state contains a CFWS
selector or literal URI reader; no second live child or source-sized scratch
is retained. One poll invokes one bounded child plus fixed checked metadata,
within 160 visits, 192 interpretation steps and 12 job records, and at most
one separately charged output byte. These ceilings are inherited from the
independently tested M06cs/M06cq child turns; the coordinator fixture pins
combined costs rather than measuring each turn. Fresh consuming phase
handoff keeps the
original owners but leaves unused prepaid credit behind; the next phase
charges conservatively without a new allowance. Complete field boundary
selection and URI/fold validation precede projection. Allocation intervals
cover combined healthy/refused fields, long values and fresh owner handoff.
No extra arena is introduced. Encoded-word dispatch, retained metadata and
worker/native/RSS qualification remain open.

M06cu's resident CID/language selector plus HeaderBudget fits 768 bytes in
the existing parser region. It borrows authorized source and stores at most
two selected passive raw fields plus a candidate and section end, with one
exclusive identifier/language syntax child,
the raw scanner and fixed progress. Each poll invokes one bounded phase;
fixtures measure at most 256 source visits, 256 interpretation steps and 16
job records with no conversion output. One credit owner persists through
malformed candidate fallback and both label kinds, so no phase discards or
renews original allowances. Whole-section success precedes visible selected
fields. Projection/retention, automatic traversal composition and
worker/native/RSS qualification remain separate; no label-value allocation,
second header arena or raw-header allowance is introduced. Warm/measured
Rust allocation intervals cover missing/malformed/duplicate fields, long
CID/list spellings, four-byte comments, source/header/nesting/job/aggregate
refusal, fresh retirement and consuming handoff. Successful handoff projects
selected CID/language with the exact returned owners, then reuses them for
another header section; no retained value/list allocation is claimed.

M06cw's bound CID JSON cursor plus HeaderBudget fits 640 bytes in the existing
parser region. It owns one CID child and one six-byte shared JSON pending
buffer, with no input-sized storage or retained string. A turn invokes at
most one CID child poll or copies six already-paid bytes. Combined bounds
are 160 source visits, 255 interpretation steps, 16 job records and ten
conversion/serialized output bytes; fixtures measure those bounds. Exact
serialized-length charges add to the existing CID conversion charges, and
short drains never renew them. Complete JSON and fresh original admission
precede consuming handoff and visible final diagnostics. Partial output
remains provisional; no null mapping, whole-response transaction or additional
source/output grant is introduced.
Warm/measured Rust allocation intervals cover long CID spellings, four-byte
comments, decomposed Unicode, escapes, noncharacter repair, syntax/nesting/job
and output refusal, fresh retirement and original consuming handoff. They
remain separate from native, worker-stack and RSS qualification.

M06cy's selected language JSON cursor plus HeaderBudget fits 512 bytes in
the existing parser region. It retains one language child, the same source
slice, one current tag extent/position and one fixed shared array frame.
There is no retained tag list, NFC scratch or input-sized buffer. Each turn
invokes one language poll, replays one ASCII byte or performs one framing
turn; bounds remain 160 visits, 192 steps, 12 job records and at most one
serialized output byte. Grammar/replay share original prepaid credit.
Replay adds exactly one original source visit and step per tag byte; exact
serialized-length charges pay quotes, punctuation and tag bytes once.
Unit fixtures measure turn and complete cost bounds. Warm/measured Rust
allocation intervals cover long tags/four-byte comments, many duplicated
tags, tiny/empty drains, malformed/nesting and mid-replay job/header/output
refusal, fresh final
retirement and original-owner reuse. Native, worker-stack and RSS
qualification remain separate; no new parser reservation or grant follows.

M06cz's retained label JSON collector plus HeaderBudget fits 1024 bytes in
the existing parser region. CID/language child phases are exclusive; backing
windows are separate caller-reserved retention storage, never grown here.
Each turn performs original zero-count admission and one transition or one
selected child poll. A completing child poll also freshly consumes its
owners and restores them without another resource charge. At most two
transition-only polls add no interpretation/output work. Per-turn selected
child ceilings remain at most 160 visits, 255 interpretation steps, 16 job
records and 10 combined conversion/serialized output bytes. Direct
backing drains do not charge paid JSON twice. Missing values consume no output
window. Capacity/resource/late syntax refusal retires the entire pair without
wiping provisional backing. Exact-grant and original-owner reuse fixtures
pin aggregate costs; a public-child prefix oracle pins every cumulative
turn cost and completion point. Checked backing bounds are 6N+2 for CID JSON
and 2N+3 for language JSON from the complete raw field-value length N; overflow
returns None. No new parser reservation or output grant follows.
Warm/measured Rust intervals cover long paired labels, absence, exact backing,
capacity/syntax/job refusal including a late language output cut, fresh final
retirement and original-owner reuse. Input/backing allocation precedes each
measured interval; native, worker-stack and RSS qualification remain separate.

The composed part-header metadata cursor plus HeaderBudget retains its
compiled 6 KiB ceiling within the existing 16 KiB parser region. Header,
label and location cursors are exclusive live owners; retention windows
remain separate caller-owned backing. Each turn performs fresh original
admission and at most one active child poll, then freshly consumes a
completing child and hands back the same owners with no added
parsing/output charge. The passive same-entity extent mapping reads no
bytes. Delegated header/label turn ceilings remain unchanged; no new
parser reservation, source grant or output grant follows. Whole metadata
stays hidden until complete healthy composition, and later capacity,
work or admission refusal hides earlier header results too. Exact
resource-grant fixtures compare exact refusal contexts with standalone
public children, require a retired owner and pin original grant
exhaustion. Every prefix deadline cut checks original-owner retirement.
A standalone header-plus-label prefix oracle compares all original
job/header balances and completion after every composition turn; handoff
adds no charge. Warm/measured Rust allocation intervals cover long
selected labels, absent body-only labels, malformed-first selection,
exact windows, label-capacity and job refusal including late language
output exhaustion, fresh final retirement and consuming original-owner
reuse. Source/backing allocation precedes each measured interval. Native
allocator, worker-stack and RSS qualification remain separate.

M06dh extends that same composed cursor with retained location. The
observed 64-bit host layout grows from 5872 to 6000 cursor bytes, plus
the unchanged 24-byte HeaderBudget: 6024 bytes total, still within 6 KiB
and the same 16 KiB parser region. These observed sizes are a host
qualification snapshot; the compiled 6 KiB ceiling is the invariant.
Location uses its separate reserved JSON window and the original
immutable Entity; scratch is parked during that exclusive child. No
positive charge, copying, renewed credit or second child poll is added
by the handoff. Delegated child ceilings remain unchanged. Metadata and
label discovery already scan the entity's headers separately; location
discovery adds another whole header scan, followed by chosen-value
replay. All repeated visits, steps, I/O and records spend the original
per-email and job allowances. A previously fitting part can therefore
now refuse; no aggregate budget is raised or renewed.

Nine units qualify complete same-source metadata, exact three-child
costs after every progress turn, all five resource cuts/exact grants,
CID/language/location output capacities, first-empty/repair/absence,
fresh prefix admission with phase error context, typed location nesting
refusal without rescue and defensive boundary-correlation refusal.
Warm/measured Rust intervals cover long literal and folded-word locations,
1024 malformed needed occurrences, empty/repair/absence, late location
capacity refusal, original triple-owner reuse and fresh complete-view
retirement. Sources and all independent backings allocate before counting;
native, RSS, worker and response reservations remain separate.

M06dj's source-bound traversal wrapper plus HeaderBudget has a compiled
16 KiB ceiling in the existing parser/boundary reservation. After fresh
traversal consumption, only the small binding and one retained metadata
child remain. Structure plus PartCursor plus HeaderBudget has a compiled
8 KiB ceiling inside that same region. The observed 64-bit host snapshot
is 11928 traversal bytes plus 24 HeaderBudget bytes (11952 total), then
72 binding bytes plus 6072 PartCursor bytes plus 24 HeaderBudget bytes
(6168 total). These are host layout observations, not native stack/RSS
or exact target ABI claims. Source, caller descriptors and all
independent response windows stay separately reserved; no table or
growing state is added.

Every bound traversal/part poll calls at most one original child poll;
constructor, checked extent mapping, body correlation and owner handoff
add no positive charge or renewed credit. Child source visits, repeated
header scans, work records and scalar/wire output spend the same
original Meter/HeaderBudget throughout. Fresh binding admission consumes
no positive allowance. Scratch remains the caller's admitted owner, used
only during serial metadata projection. A later failure hides all child
views without wiping backing. Part construction marks the binding
Abandoned; only successful consuming finish restores it, so dropping or
safely forgetting also leaves it retired.

Seven units qualify the binding. One compares same-source clipped
metadata, context, exact original pointers, costs and poll counts
against independent public children at zero, nonzero and maximum-fitting
bases. The other six use base 17 to cover every traversal prefix and
both root and digest-child metadata prefixes, fresh-before-premature and
whole-binding admission, ordinal/capacity/drop/forget refusal, defensive
body/range/identity checks, inert cached completion and explicit
traversal admission. Three exclusive owner types have active Copy/Clone
compile-fail guards. Warm/measured Rust intervals qualify nested/long/
folded/malformed metadata, original-owner reuse, late capacity and fresh
whole-binding retirement. Inputs and backing allocate before counting;
worker, native and RSS qualifications remain separate.

M06dk adds no retained cursor state or parser arena. Bound
classification consumes complete original metadata, then spends 64
source visits and 64 steps through the same HeaderBudget and its
original job I/O/work funding. Class::from_headers charges Class
retention; finish_classified additionally charges the compile-time
difference between Node and Class sizes before exposing a whole node.
The existing Part + Node + membership compiled ceiling stays 64 bytes
per admitted body slot. Caller node/list backing stays separately
admitted; no second descriptor or metadata table is introduced.

Five units qualify compact classification. One compares literal original
parent/depth/classification and exact five costs against independently
consumed public headers plus Class at zero, nonzero and maximum-fitting
bases. Three others hand a nested mixed/alternative tree to existing
body-list selection under original owner pointers, check fresh
classified finish at every root/digest-child metadata prefix and require
typed refusal at all five resource cuts against independent whole
traversal, metadata and classification costs. Output cuts reach both
Class and extra Node admission and pin remaining bytes. The fifth
preserves original retained metadata and ordinal beside its node.
Warm/measured intervals cover serial nested plain/HTML classification,
named inline media, an attachment with an asserted decoded UTF-8
extended filename, original owner reuse into body lists and late fresh
part refusal. Inputs and caller backing precede counting; native, worker
and RSS reservations remain separate.

M06dl keeps Classifying plus one ordered Part plus HeaderBudget below a
compiled 8 KiB ceiling within the existing parser reservation. Completed
Classified alone has a compiled 128-byte ceiling. The observed host
assembly snapshot is 96 Classifying, 6096 ordered Part, 88 Classified
and 24 HeaderBudget bytes: 6216 live construction bytes including
admission. These are host layout observations, not exact target ABI,
native stack or RSS promises. The original source, caller
Part/Node/membership slots, scratch and metadata/list backing remain
separately admitted. No second traversal frame set, metadata table,
growing state or new descriptor reservation is added. Ordinal mapping,
slot writes, progress advance and healthy owner handoff add no positive
charge or renewed credit; original child parsing/classification/output
costs are unchanged.

Four units qualify the ordered owner. One compares exact five costs,
child poll counts, original pointers, complete descriptors and literal
nodes with independent bound projections at zero/nonzero/maximum-fitting
bases, including forged prefilled slots and untouched spare backing.
Three others cover every completed-part prefix, every
root/digest-child/explicit-leaf metadata progress prefix,
fresh-before-premature and fresh complete-owner finish,
capacity/premature/extra refusal and unfinished/completed safe
forgetting. Total/completed queries, healthy owner/part admission,
late construction-owner checks and already-retired Structure admission
are pinned too. Three exclusive ordered types have six active Copy/Clone
compile-fail guards. Counted Rust intervals cover healthy
whole-owner release, partial/completed late admission, serial
original-owner reuse, capacity failure and completed-child forgetting
with all caller backing created before counting. Native, worker and RSS
qualification stays separate.

M06dm's original-source Selecting plus HeaderBudget has a compiled 16 KiB
ceiling in the existing parser reservation; completed Selected has a compiled
256-byte ceiling. The existing body-list cursor supplies the only 65-frame
set. Original source/base and complete Part/Node slices stay privately bound;
caller lists and membership retain their existing separate admission. No new
metadata table, backing grant, parser or per-container allocation is added.
The wrapper adds no positive charge: direct-selector turns and all five cost
counters remain identical, including unchanged header byte/step counters.

One unit compares independent supplied-node selection with bound completion
at zero/nonzero/maximum-fitting bases, literal digest and nested alternative
fallback lists, original source/cells/owners, prefilled nodes and spare backing.
Three further units cover every list progress prefix, cached Complete versus fresh finish,
late selected-owner retirement, constructor admission priority and local limits, sticky original
header refusal, premature finish, each job counter cut and all four output
window failures. Four exclusive-owner Copy/Clone guards and a rejected passive
constructor compile-fail guard pin ownership. The harness-free Rust allocation
probe includes healthy complete original-owner release, partial deadline,
complete-owner refusal, membership/text capacity and completed-owner forgetting
with all backing created before the counted interval. Native and RSS evidence
remain separate.

M06dn response metadata replay reuses the original source, header limit,
classified nodes and selected lists, without another retained metadata table.
Projecting plus its sole Part child and HeaderBudget has a compiled 8 KiB
ceiling within the existing parser reservation; Projected has a compiled
256-byte ceiling. Observed x86-64 host layouts are Projecting 152 bytes,
Part 6088 bytes, HeaderBudget 24 bytes, totaling 6264 bytes; Projected is
144 bytes. These are compiler layout observations, not native-stack or RSS
measurements. Per-part windows and Scratch are reused serially under
original owners. Replayed metadata incurs the existing source/interpretation
and output work; node classification and its retained-node output are not
repeated. An independent metadata comparison pins turns and all five cost
counters at three bases for digest and nested-alternative fixtures. Further
units cover every digest part progress prefix, whole completion, late
constructor/next/complete-owner admission, capacity, extra parts, safe
forgetting and original job/header cuts. Six exclusivity guards and a
passive-constructor rejection pin the owner boundary. The existing allocation
probe retains all prior classification/selection intervals and additionally
counts healthy replay release, partial/whole/completed-owner deadline
refusals, location capacity, abandonment and constructor refusal. All backing
exists before counting. This remains Rust allocation evidence; native-stack
and RSS qualification are separate.

M06do metadata member framing consumes the live original replay child before
its caller window can be reused. The metadata parser and JSON framer never
coexist as live cursors. JSON Cursor plus HeaderBudget has a compiled 1 KiB
layout ceiling, including one shared six-byte string-frame buffer and a
20-byte primitive decimal buffer. It uses no heap or source-sized storage.
The previously admitted metadata windows and Scratch remain borrowed.

Every turn reads at most one UTF-8 scalar (four bytes), inspects at most five
text-type bytes, copies at most 64 bytes, or performs a fixed transition.
Measured turns consume at most five source/I/O bytes, 21 interpretation
steps, two records and 64 output bytes. Generated digits/defaults are not
source I/O. Primitive u64 formatting has a
precharged 20-digit bound. Original work/header owners pay for scalar rereads
and control work. Every byte of the new wire fragment is charged before
copying, including retained CID/language/location JSON from its original
producer. Metadata retention and fragment emission are separate operations.
No repeated parsing, renewed allowance or new arena is introduced.

The dedicated Rust allocation probe preserves all 21 previous ordered MIME
trials and adds six JSON trials: healthy original owner release, partial and
complete deadline refusal, partial and complete safe forgetting, and fresh
constructor refusal. Caller output and metadata storage precede counting.
Units cover every digest-child serialization prefix and original quota
cutoff, short/wide drains, long retained JSON, all scalar widths, exact wire
debits, final charset mapping and unsigned-size bounds. These are Rust-heap
and compiler layout checks, not native-stack or RSS qualification.

M06dp fixed fragment retention binds the original live replay child to
one separately admitted output window. It privately constructs the
existing framer and uses the shared Window for exact reported prefixes.
Cursor plus HeaderBudget has a compiled 1 KiB ceiling; no new heap
storage, parser or serialization algorithm is introduced. Each turn
retains the existing one-scalar/64-byte bound and adds no wire debit or
allowance reset.

The dedicated Rust allocation probe preserves all 27 earlier ordered
MIME and framing trials, then adds seven retention trials: healthy owner
release, partial/complete deadline refusal, capacity refusal,
partial/complete safe forgetting and constructor deadline refusal. Its
distinct caller response window precedes counting. Units pin exact-fit
and every-shorter capacities, bare-framer bytes and all five cost
counters, original caller owner/backing pointers, every digest-child
prefix, escaped/long-raw capacity cuts and sticky resource refusal. Full
midstream windows preserve deadline priority. These checks do not
qualify native stack or RSS.

M06dq whole fragment collection uses one separately admitted Cell and
output window per original ordinal. Metadata and framing/retention
phases are serial and cannot escape their original slot. Collecting plus
Child and HeaderBudget fits a compiled 8 KiB ceiling; Serialized fits
256 bytes. Cell size is compile-checked at 128 bytes. Original
allowances fund two steps and three records per slot, including the
stored-cell record, with no source/I/O/wire debit. Collection does not
copy or charge fragment bytes again. No source-sized internal buffer or
heap is introduced.

Rust allocation qualification preserves all 34 earlier trials and adds
seven collection trials: healthy original release, partial deadline,
completed-owner deadline, capacity, partial/complete safe forgetting and
constructor expiry. All cell output storage exists before counting.
Units pin original preorder/selection/backing/owners, every first-child
progress prefix, every cutoff of five original counters,
incomplete/extra/reused cells, capacity and fresh whole completion.
Seven compile-fail guards pin exclusive owners and reject passive
completed visitation as constructor input. These checks do not qualify
native stack or RSS.

M06dr body metadata composition uses 64 inline parent ordinals with no
recursive traversal or growing storage. Cursor plus HeaderBudget fits
1 KiB; exclusive Composed fits 256 bytes. Caller output is separately
admitted. Each active nonempty-output turn funds at most one step, one
record and 64 new wire bytes; retained generated input incurs no source
or I/O charge. Empty output freshly admits without debit/progress; cached
Complete is inert. No additional allowance or normalization buffer enters.

Eight measured Rust allocation trials preserve the earlier 41: healthy
tree/list release, partial expiry, completed-owner expiry, empty-output
expiry, partial/complete forgetting and constructor expiry. All backing
storage precedes counting. Units cover output widths, exact capacity,
original owner identity, every prefix and quota cutoff, and depth 64;
five compile-fail guards reject copying, cloning and passive substitution.
These fixtures do not qualify native stack, RSS or full JMAP responses.

M06ds whole body-member retention uses one separately admitted fixed
window. Cursor plus HeaderBudget fits 1 KiB; exclusive Retained fits
256 bytes. Shared Window adds no heap, source-sized internal buffer,
allowance or repeated wire charge. The fresh private composer preserves
all five bare-composer costs, including generated-byte billing.

Eight measured Rust allocation intervals preserve the earlier 49:
healthy tree/list release, partial expiry, completed retained expiry,
capacity refusal, partial/complete forgetting and constructor expiry.
All output backing exists before counting. Units pin exact-fit and every
shorter capacity, original window/tables/owner identities, every prefix,
fresh whole release, deadline priority and every step/record/wire cutoff and zero-source/I/O debit.
Six compile-fail guards pin exclusive ownership and reject advanced or
emission-only substitution. Native-stack and RSS qualification remain
separate; this does not qualify complete authenticated response memory.

For N original parts, a conservative Structure window bound is 16 plus
sum(fragment length + 19) over all N parts. For Lists, use 66 plus that
sum over every list occurrence, including duplicates across text/html/
attachment lists. The constants cover member names, object/subParts
framing, list delimiters, separators and hasAttachment. Compute bounds
with checked arithmetic before separately admitting the window. This
copy overlaps all original fragment cells until release; include both
in admission. A short window consumes the original completion proof;
retry needs newly reconstructed traversal/collection under admission.
Every healthy incomplete composer state still owes a byte, so capacity
precedes the next funded step; fresh deadline admission precedes both.
Partial/complete forgetting exercises loss of proof and is equivalent to
dropping these destructor-free owners; no destructor cleanup is claimed.

M06dt direct-leaf candidate mapping uses one separately admitted fixed
Candidate slot per original part. Candidate fits 128 bytes, so its
conservative reservation is 128 times the admitted part count, computed
with checked arithmetic. Slots overlap all original retained metadata
until release. Cursor plus HeaderBudget and one temporary Candidate fits
1 KiB; Mapped fits 256 bytes. No heap, new allowance or body buffer enters.

Each turn maps one ordinal and funds at most 69 steps, six records and
69 new wire bytes before storage, with no source/I/O debit. Multipart
turns fund one step and a stored record but emit no locator. Candidate
wire uses encoded length, not decoded-size storage or an extra replay.

Eight measured Rust allocation intervals preserve the earlier 57:
healthy tree/list release, partial expiry, completed Mapped expiry,
partial/complete forgetting, too-few slots and constructor expiry. All
candidate backing precedes counting. Units pin literal transfer tags,
encoded extents, source-base normalization and every positive quota cut;
zero source/I/O allowances still succeed. Five compile-fail guards pin
exclusive owners and reject passive substitution. Destructor-free
forgetting tests consuming loss of proof. These checks do not qualify
native stack, RSS, parent authorization or complete response memory.

M06du original source binding consumes Mapped plus the already admitted
complete PinnedBlob; no new body backing or allowance is constructed.
Every turn hashes at most 4096 resident bytes and charges that many
original work I/O bytes, one interpretation step with carried control
credit and one explicit record before update. Thus a turn uses at most
two records and no new wire/header-source bytes. This counts an additional
resident source visit, not a filesystem read or a renewal of input work.
The Provider cursor plus HeaderBudget and final 32-byte digest fits
1 KiB; Bound fits 768 bytes. The constructor checks that same combined envelope for generic digest
state. All original resident source, retained metadata and
candidate slots overlap the live pin's existing pooled-view reservation.
The actual query clock is fenced before/after crypto work and freshly
on explicit checks and consuming release. Terminal refusal hides views
and drops digest state. The earlier 65 measured metadata/locator trials
remain unchanged; binding-path Rust/native allocation, stack, RSS and
complete response/service bounds are not qualified by this increment.

M06dv adds eight dedicated sequential Rust allocation intervals around
original source-binding construction, funded digest turns and consuming
release/refusal. Real complete PinnedBlob and original Mapped belong to
one test-only compilation of production sources. Cold filesystem setup,
pin verification, resident-source construction, original mapping and all
backing are outside counting; binding ownership and PinnedBlob descriptor
teardown are inside. Enclosing PooledRead/CommittedView and scratch lease release
remain outside. Healthy zero/near-u64 bases, partial/completed/Bound expiry,
same-length source mismatch, original I/O refusal and constructor expiry
leave every Rust allocation counter unchanged. Only these eight intervals
are qualified; the remaining source-binding cases listed in API §1.127
are unmeasured. Existing 65 metadata/locator trials remain separate. Native allocation, worker stacks, RSS,
complete request admission and access-policy/publication wiring remain
unqualified.

M06dw original source-bound part-member emission consumes only Bound and
retains its actual descriptor plus all original source/windows/candidate
backing. One funded turn uses at most 64 new wire bytes, one interpretation
step with carried control credit and one explicit record; there is no
source/I/O debit for retained generated bytes. Cursor plus HeaderBudget,
five borrowed segment descriptors and 64 output bytes fit 1 KiB; Member
fits 768 bytes. These are size caps, not enforced ledger reservations.
Eight sequential Rust allocation intervals measure construction through
consuming emission/release/refusal and descriptor teardown after cold
mapping, source matching and fixture setup. Leaf/container success,
partial/completed/Member expiry, wire refusal, invalid ordinal and
constructor pin expiry preserve every counter. Enclosing pooled-view/
scratch-lease release stays outside counting. Existing 65 metadata/locator
and eight binding intervals remain separate. Native/stack/RSS, complete
request admission, current access policy and publication remain open.

M06dx whole source-bound part-member retention constructs its emitter
fresh from original Bound and advances a shared fixed Window only by
reported bytes. It adds no wire charge or replacement allowance. A
checked fragment length plus 81 bounds the largest blobId suffix and
covers multipart null; that separate caller reservation overlaps every
original source/window/candidate and actual descriptor/pool borrow.
Cursor plus HeaderBudget, five borrowed segments and 64 output bytes
fits 1 KiB; Retained fits 768 bytes. Complete ledger and worker-stack
bounds remain open. Eight sequential Rust allocation intervals measure
construction, retention and descriptor teardown for near-u64 leaf and
zero-base container success, partial/completed/Retained actual-pin-clock
expiry, short window, wire refusal
and constructor expiry. Backing, mapping, source matching and file setup
precede counting; enclosing pooled-view/scratch-lease release stays
outside. Only these eight intervals are qualified; API §1.129 records
remaining unmeasured boundaries. Existing 65 metadata/locator, eight
binding and eight member intervals remain separate.

M06dy whole original source-bound collection overlaps original Bound,
actual descriptor, prior source/windows/candidates and separately
admitted member cells. Each child retains one fresh emitter/window;
acceptance starts fresh zero credit and adds exactly one original
interpretation step, one prepaid control record and one explicit record
(two work records), with no new wire or source/I/O debit. Collecting plus Child plus
HeaderBudget fit 2 KiB; Cell fits 64 bytes and Serialized fits 768 bytes.
These are size caps, not whole-ledger or worker-stack proofs. Eight
sequential Rust allocation intervals cover healthy leaf/multipart,
partial/completed child expiry, completed parent/Serialized expiry,
abandonment and constructor expiry. Mapping, matching, file/backing
setup and cell-array allocation are cold; descriptor teardown is
counted, enclosing pooled-view/scratch lease and fixture cleanup remain
outside. API §1.130 names remaining unmeasured variants. Earlier 65
metadata/locator and eight each source-binding, emission and whole
retention intervals remain separate. Native/stack/RSS and complete
admission/publication qualification remain open.

M06dz source-bound whole tree/list emission shares bounded framing with
metadata emission and retains original Serialized, all source-bound
member cells and actual descriptor. Every turn funds one original
interpretation step with carried control-record credit and at most 64
new wire bytes, with no source/I/O debit. Cursor plus HeaderBudget and
64 output bytes fit 1 KiB; Composed fits 768 bytes. These size envelopes
do not prove whole-ledger or worker-stack bounds. Eight sequential Rust
allocation intervals measure digest-tree Structure and digest Lists,
actual-pin-clock expiry after a copied prefix, completed Cursor/Composed
expiry, released Serialized explicit-check expiry, wire refusal and
constructor expiry. Original mapping/matching/collection, backing and
file setup stay cold; descriptor teardown is counted, enclosing pooled
view/scratch lease, cell array and fixture cleanup stay outside.
API §1.131 lists unmeasured variants. Existing 65 metadata/locator and
eight each binding, member emission, whole retention and collection
intervals remain separate. Native/stack/RSS and complete request
admission/authorized publication remain open.

M06ea whole source-bound tree/list retention overlaps original complete
collection, source/fragments and member cells, original actual pin and
the separately admitted whole generated property-member window. No new
source/I/O or output/work fee accompanies retention; the private fresh
composer funds each turn. Cursor plus HeaderBudget fits 1 KiB; Retained
fits 768 bytes. These compiled caps are not full-ledger or stack proofs.
Eight sequential Rust allocation intervals measure near-u64 digest
Structure, zero-base digest Lists, actual-pin expiry after a copied
prefix, completed Cursor/Retained expiry, released Serialized explicit
check expiry, empty-window capacity refusal and constructor pin expiry.
Original matching/collection/backing and whole output preparation stay
cold; descriptor teardown is counted, pooled view/scratch lease, cell
array and fixture cleanup stay outside. API §1.132 lists unmeasured
variants. Earlier 105 intervals remain separate evidence. Native
allocation, worker stack/RSS, full request admission, selection and
current authorized publication remain open.

M06eb requested source-bound list selection overlaps original complete
collection, source/fragments and member cells with the actual descriptor
and small pure selection. One shared Frame produces only requested keys;
full-list callers retain prior ALL bytes/turns/fees. Each unfinished
nonempty turn funds one original step and at most 64 actual wire bytes;
empty selection freshly admits but generates zero bytes and charges no
new work. Cursor plus HeaderBudget and 64 output bytes fits 1 KiB;
Composed fits 768 bytes. These size caps are not whole-ledger/stack proofs.
Eight sequential Rust allocation intervals cover near-u64 digest textBody
only, zero-base empty selection, actual-pin copied-prefix expiry,
completed Cursor/Composed expiry, released Serialized explicit-check
expiry, wire refusal and constructor pin expiry. Original mapping,
matching, collection/backing, full verification and file setup stay cold;
descriptor teardown is counted, pooled view/scratch lease, cell array and
fixture cleanup stay outside. API §1.133 lists unmeasured variants.
Earlier 113 intervals remain separate. Native/stack/RSS, full request
admission, selected retention/routing and current publication remain open.

M06ec source-bound selected retention adds a compiled Cursor plus
HeaderBudget cap of 1 KiB and Retained cap of 768 bytes. Whole output
remains separately caller-owned/admitted. The cursor holds the original
source-bound composer, shared Window and pure selection; retention adds
no interpretation, record, wire or source/I/O debit to selected emission.
Its eight allocation intervals
measure near-u64 textBody, empty selection into zero capacity, copied
prefix actual expiry, completed Cursor/Retained expiry, explicit checked
released Serialized expiry, nonempty zero capacity and constructor pin
expiry. Original preparation and enclosing resources stay outside;
descriptor teardown is counted. API §1.134 states exact scope and
unmeasured variants. Native/stack/RSS/full admission remain unqualified.

M06ed requested tree/list routing preserves the compiled 1 KiB Cursor
plus HeaderBudget/output cap and 768-byte Composed cap. API §1.135
names eight Rust allocation intervals, including near-u64 tree+textBody
and empty selection, with original preparation/enclosing resources
outside and descriptor teardown counted. Earlier 129 intervals remain
separate; native/stack/RSS/full admission are unqualified.

The resident unstructured header cursor fits 208 bytes, including its source,
UTF-8 state, current word decoder and raw replay offsets, within a future
256-byte decoding checkpoint. It uses the same decoder region with no
candidate, whitespace or decoded-header buffer. Allocation intervals cover
folding, words, malformed literals/payloads, whitespace retention and copied
replay. M06t adds a checked turn ordinal within the same 208-byte bound and
composes this state into each of four NFC source checkpoints, each at most
256 bytes including pending decomposition. The complete normalizer cursor
plus aggregate budget stays within the 1 KiB checkpoint reservation alongside
3 KiB segment/count scratch. Combined allocation intervals cover fast and
replay paths with charged output. Worker-stack bounds remain open.

The header property selector fits 128 bytes and its borrowed result fits 48
bytes in the HTTPS slot's existing 96 KiB framing/output scratch. It owns no
field-name buffer or collection; decoded keys borrow the request arena
above. Fixed alias/form tables add no per-job allocation; long request names
scan in bounded turns. Isolated allocation intervals cover standard aliases,
parameterized all-occurrence selection, long names and syntax/form
rejection. Complete worker stack accounting remains open.

Body charset prescan fits 64 bytes in the existing 2 KiB decoder/HTML/snippet
state and future decoding cursor checkpoint. It borrows caller fragments
and retains no source, body copy or output buffer. Its isolated allocation
interval covers label selection, single-byte fragments, valid/malformed body
completion, copied-state replay and fresh-meter refusal with unchanged Rust
counters. These measurements cover the standalone scan; the composite owner
below owns transfer replay. Full body worker stacks remain unqualified.

The transfer-to-charset Reader fits 512 bytes including its inline transfer
Reader, charset/prescan state and one pending byte. It uses one existing
6 KiB source partition and its 2 KiB checkpoint backing, reserving slot zero
for rewind. Its inline state fits the conversion region's existing 2 KiB
decoder/HTML/snippet state; no new ring, body copy or process allowance is
added. A synthetic-source allocation interval covers two-pass base64 text,
transfer diagnostics, replay I/O failure and sticky work refusal without
changing Rust counters. Combined filesystem allocation, nested-source use,
whole worker stacks and service RSS remain unqualified.

The plain body-value filter fits 64 bytes alongside the text reader in the
same 2 KiB decoder/HTML/snippet state. It retains a possible CR, byte counts
and flags, never a text buffer. Its allocation interval covers conversion,
noncharacter replacement beyond a cap, truncation/completion, pending-state
replay and fresh-meter refusal. A functional composed test validates a
malformed charset tail after the output cap. Response-spool allocation,
HTML handling and complete worker stacks remain unqualified.

The 32 KiB conversion region has this fixed simultaneous partition: 2 KiB NFC
segment cells (256 cells), 1 KiB class counts, 1 KiB NFC source checkpoints,
16 KiB Unicode token scalars, 8 KiB u16 KMP prefix entries, 2 KiB for 64 token
descriptors, and 2 KiB decoder/HTML/snippet state. Simple lowercase maps one
scalar to one scalar, even when UTF-8 lengths change. Compile and evaluate
one token at a time, at most 4096 scalars; reuse the pattern/prefix arrays for
the next token and charge rescans. Token descriptors retain request/spool
extents rather than copies. Store matched-token bits in those descriptors.
NFC and a matcher may run together within this partition; do not allocate a
whole-query automaton. HTML retains bounded lexical state, not an element
tree or arbitrary attribute string. Snippet context/escaping uses the 2 KiB
state area in phases; exact match spans use the separately admitted disk file
in POLICY.md. Fairness yields after at most 256 decoder/matcher transitions,
including prefix fallback work, not just after successful input consumption.
Thread candidate rings retain 64 source extents, not 64 owned 1004-byte IDs;
materialize one lookup key at a time within existing parser/read scratch.

The 128 KiB queue/state reservation has eight 64-entry input queues and one
512-entry completion queue, all with 32-byte entries (32 KiB total), a
128-entry due-recipient window with 128-byte entries (16 KiB), 64 reservation
records with 128-byte entries (8 KiB), one future dedicated
checkpoint-attempt record plus sequence counter (at most 512 bytes), and a
remaining 73,216 bytes for timers, other pools' slot generations, bounded
generation pins, queue heads and counters. A
reservation record stores references/charges; it does not embed a frame.
The window can be refilled from the disk due-time index, never from a full
in-memory queue. Scratch/queue bounds apply even with configured larger pools.

Each 1 MiB configuration snapshot permits a 512 KiB text/secret arena,
4096 alias descriptors of at most 32 bytes (128 KiB), and 384 KiB for domain,
identity/device/endpoint descriptors, resource plans, indices and ownership
metadata. Routing uses 320 KiB of the text arena, leaving 192 KiB for other
configured text and decoded credential material. Its 256 domain descriptors
use 4 KiB of the descriptor region; each alias descriptor remains 32 bytes.
Domains are stored once and aliases retain local text plus domain indices.
Their conservative text bound is 324352 bytes, below the 320 KiB reservation.
These partitions are not extra allocations. Combined arena bytes still bound
other configured text. Build a new snapshot from the bounded stream scratch;
do not keep an extra file-sized input copy beside both snapshots. CONFIG.md bounds the
physical input at 2 MiB, streamed through the control worker's existing 64 KiB
configuration region: 16 KiB input chunk, 8 KiB physical line, 4 KiB decoded
string, and 36 KiB parser/builder working state. The scalar resource builder
fits within 4 KiB of that working state, including fixed source locations for
four sections of at most 64 fields each. No input-sized syntax tree is
retained. Certificate/key provider
allocations belong to their separate TLS/certificate entries. Pin old snapshots
only for bounded operation lifetimes and reauthorize Access as API.md specifies.
SCHEMA.md fixes the remaining target stanza/field ceilings and the 384 KiB
metadata partition. Temporary borrowed identity views use 80 KiB of the existing
256 KiB control-worker stack, outside the snapshot owner, as SCHEMA.md specifies.
Implemented configuration tables and temporary view arrays carry concrete
size/capacity guards. Device cells and runtime consumers remain incomplete;
their implementations must establish corresponding bounds before use.
The 228 KiB remainder is within the same reservation,
not additional process memory.

This ledger does not budget whole earlier JMAP responses, generic JSON trees,
or all MIME body values in memory. ADMISSION.md defines bounded private response
spools/result references and their work/disk admission. POLICY.md defines
parser/charset/search/thread policies and CASES.md the fixture inventory.
Concrete implementations must fit these contracts before enabling service.

## Evidence

M04b2c3d4b's portable integration test provides point-in-time evidence for
its compilation of the structural loader on a worker whose non-growing guarded mapping is checked to fit 176 KiB, leaving
the existing 80 KiB identity-view reservation untouched. CONFIG.md lists its
fixtures, manual requalification obligation and limits. A production
compile-time guard checks borrowed-view and list-range layouts on every target.
The structural fixture does not call the implemented preimage assembly or
create its finalization arrays. It does not qualify future protected-file,
provider or runtime frames, or claim measured service RSS.

M04b3b2b2a separately qualifies test-compiled structural loading, text-input
materialization and preimage assembly on a non-growing guarded mapping of
at most 256 KiB. This includes the temporary view arrays; it adds no stack
reservation. CONFIG.md lists maximum-table/text and failure/reuse fixtures.
Future M05/M07 adapters and installed service callers require their own
qualification. Neither fixture measures hot-path allocation or service RSS.
No ledger entry or worker count changes.

M07e's first stack fixture runs twenty-five existing compiled-policy and retained
transport scenarios sequentially on one requested 240 KiB worker stack. The
portable release artifact must observe a read/write mapping no larger than
256 KiB, preceded by a contiguous inaccessible guard of at least one page and
without Linux's grow-down flag. This includes policy construction/refusal,
client-only to complete generation replacement, direct and gateway TLS, both
STARTTLS owners, deadlines, tails and cancellation. The gateway peer is a
separate synthetic process and its own stack is outside this observation.
The role scenario uses inline construction on this checked worker; its ordinary
unit test separately retains the cross-thread ownership check.
A non-ignored host test runs the same complete scenario set to detect fixture
drift, including inline construction; its host stack is not target evidence.

This checks those test-compiled paths against the existing worker allowance;
it neither measures peak used stack bytes nor qualifies every provider/CPU
path, maximum configuration/certificate layout, production opener, scheduler
frame or simultaneous worker. Initial and later RNG work for this one worker
is included in its exercised paths; per-worker native heap state is not
measured. Rust/native allocation, complete generation/session coexistence,
whole-process RSS and the known session-cap sum remain admission blockers.
The retained-connection scenarios also exercise implicit handoff tails, tightened
handshake deadlines, sticky failure, gateway reload refusal, established clock
failure and authorization/permit release. Successful execution on the observed
mapping does not prove native stack probing: the pinned native flags do not
enable stack-clash protection. This fixture cannot establish that every native
overflow would fault on the guard. Native-frame auditing/probing remains required
before claiming an enforced end-to-end stack bound.
The fixture adds no runtime thread or reservation, dependency or unsafe code.
The existing configuration fixtures share the exact mapping parser so their
guard/growth rules cannot drift; their labels and ceilings remain unchanged.

M04c1's `admission.rs` validates the separate u64 disk/work plan and capacity
relationships in ADMISSION.md. It consumes an already validated ResourcePlan
without changing this RAM ledger. Logical upload/queue quotas may be smaller
than message_bytes; admission must enforce the intersection of logical quota,
raw-body quota and successful I/O for each operation. These settings do
not guarantee that any particular upload/submission can currently fit.

M04c2's admission work/timer helpers charge fixed scalar counters and compute
checked deadline budgets without allocation. Runtime callers own clock
sampling, fixed scheduling steps, protocol transitions, nested-budget charging
and completion of admitted durable work. These helpers do not establish
whole-process memory usage or execution-time enforcement.

Logical reservation records and grouped quota checks use fixed caller-owned
cells and linear effect tickets. Each cell plus slot fits within 128 bytes;
there is no physical-filesystem binding or probe table. The scalar writer
ledger uses the same cells for journal bytes/operations, with fixed stack
projections and no heap growth. Its barrier preserves pending frame ownership
and stays closed after selection until runtime reconciliation exists. M08 owns
building/retention accounting in a dedicated attempt outside client slots,
actual cleanup, writer/view locking and publication authority.

The std directory adapter owns a File and a 383-byte path buffer per retained
directory. Complete operational paths are bounded at 383 bytes and data roots
at 254 bytes. One child lookup uses a fixed 383-byte scratch buffer; no PathBuf
or heap string is created by the adapter. The default allocation probe covers
short and maximum-length successful/missing std paths after its positive
controls. This qualifies the pinned host/musl implementation, not every std
version. Recheck with compiler changes. Directory iteration and future mutable
file operations need their own allocation evidence before hot-path use.

The startup-only LOCK acquisition uses a typed generated name and fixed path
buffer and retains one additional File inside LockedRoot. No per-request
lock-file open or descriptor cloning is permitted. Cooperative
process locking does not alter the request pool or worker ledger.

Each live TemporaryFile/SyncedTemporary retains two Files (output and its
parent directory), one generated Name, account ID, counters and
a borrow of the existing LockedRoot. Writes and reads use caller slices without
buffer growth; path assembly uses the directory adapter's fixed byte ceiling.
These operations do not acquire quota or runtime pool slots. The dedicated
Rust allocation probe compiles the same filesystem source with its cfg(test)
fixture. That fixture supplies a root for the namespace-mapped test identity;
it does not alter production root admission. It still takes the real writer
lock and executes the actual creation, policy, I/O and sync paths. The imported
module permits unused items in this second compilation, including omitted
libtest helpers; normal library and unit-test builds remain the lint authority.

At both short and maximum 254-byte root paths, the measured interval covers
exclusive typed account/checkpoint/shard directory creation and sync, existing
directory collisions and refusal of file entries passed as directory requests;
exclusive create/prepare, 4 KiB write/read/sync/drop, existing-name collision,
byte-limit and read-offset refusal, missing ancestors, injected open/preparation
and sync failures, Interrupted attempt exhaustion, retired output refusal and
unexpected early EOF. Blob publication adds link/sync/unlink/sync success,
collision and errors before/after every effect boundary, plus bounded reads. Fresh metadata publication additionally
covers table, manifest and journal destinations with maximum numeric components,
including success and collision refusal at both root bounds.
Expected CURRENT preparation retains two fixed 120-byte encodings, an optional
previous marker and AccountId. Replacement writes directly from that retained
encoding and reads through 120-byte scratch plus a one-byte EOF buffer. It uses
fixed source/target paths and the shared temporary constructor's path scratch.
The expected-state read briefly opens CURRENT. At rename, the call retains
source and target parent Directories and the temporary's two Files (four Files
in addition to LockedRoot's retained root directory and LOCK). Each Directory
also retains its bounded path. The expected-state read bounds explicit extent
calls to 64 plus one EOF call. The measured interval includes initialization,
replacement, stale/absence refusal and injected errors before/after each file
sync, temporary-parent sync, rename and final parent sync at both root bounds.
Fixture account/directory preparation stays outside that interval.
PublishedFile retains one File, one Name, completed length and the LOCK borrow;
publication uses fixed source/destination paths and transient directory handles.
It requires unchanged Rust allocation/deallocation
counters after setup and before cleanup. No new allocator hook or production
fixture constructor is introduced. Repeat on host and static musl when compiler
or filesystem adapter code changes. This does not measure kernel page cache,
native-library allocations, directory enumeration or the whole service RSS.
Admission integration remains required before activation.

Each StoreReader/CompleteFile owns one File and generated Name, scalar extent
(and reader progress/failure fields), plus the existing LOCK borrow. Opening
uses fixed path buffers and transient parent handles. Reads use caller slices
and return after one explicit operation of at most 64 KiB. Completion adds a
length query and a one-byte EOF probe. The same allocation interval exercises
complete reads, random reads, ceiling/role refusal, injected read errors, early
EOF and completion-probe failure at both root bounds. No per-file heap buffer,
new allocator hook or runtime pool is introduced.

BlobInput retains one StoreReader, fixed provider digest state and supplied
account/ID/row fields. It hashes directly from caller read storage with a
64 KiB step ceiling. No whole-blob buffer or additional arena is reserved.
CompleteBlob retains one CompleteFile, account/ID/kind and observed digest.
The existing allocation interval measures complete reads/digest/EOF, random
reads, admission and premature-finish refusal, checksum mismatch and sticky
I/O failure at both root bounds. Fixture publication precedes measurement.
It grants no runtime pin or whole-service memory qualification.

Active-overlay loading reuses the admitted journal arena and Cell array;
frames exclude the 96-byte header kept on stack. It retains one CompletePrefix,
borrowed Overlay and ViewIdentity with no additional frame buffer or pool.
A single 128-read budget covers header and captured payload; each read is at
most 64 KiB, so a full 4 MiB payload needs 64 full reads plus the header. Short
reads consume the same budget, and exhaustion returns WouldBlock. Validation
and in-place sorting are one bounded synchronous work unit, admitted in full
with deadline checks around it. The existing allocation interval covers empty
and populated loads, tombstone lookup and byte/slot refusal before and after I/O
at both root bounds;
fixture creation and arena/cell preparation precede measurement. This establishes
primitive allocation behavior, not full-service RSS or runtime pin ownership.

Selection loading uses caller-owned scratch totaling 5032 bytes: FORMAT (80),
CURRENT (120) and maximum manifest (4832). It retains at most one input File
at a time in addition to the root and LOCK; opening uses the same transient
parent handles and fixed path scratch as StoreReader. Container decoders use
bounded stack state and the existing allocation-free SHA-256 provider. Each
file permits at most 64 explicit extent reads plus one EOF probe. Selection
borrows the scratch manifest, preventing its reuse until that borrow ends.
The allocation interval loads literal metadata and exercises missing-account
refusal at short/maximum roots; fixture bytes/directories and scratch are
prepared before measurement. This does not instantiate recovery workers or
prove full-service RSS.

TableInput borrows one 66608-byte record buffer and the selected manifest,
retains one StoreReader and the fixed table verifier (1024-byte previous key,
scalar counters/header and provider digest state). Opening uses a 112-byte
header buffer, shared fixed path scratch and transient parent handles; it
checks the manifest descriptor against an explicit admitted file-byte ceiling.
Header input permits 64 explicit reads; each record shares a fresh 64-call
allowance across prefix/remainder. No whole-table buffer or count-sized array
is allocated. CompleteTable retains one CompleteFile and Summary. Existing
writer/read-view scratch must supply these buffers before worker activation.
The measured interval streams all eleven table tags, including a populated
blob record, completes them, and exercises byte-ceiling and premature-finish
refusal at short/maximum roots. Literal fixture loading and buffers precede
measurement; this does not add allocator instrumentation or service pools.

TableReplay owns that existing TableInput and fixed inline Merge state while
borrowing LoadedOverlay. It introduces no arena or pool: the table continues to
use caller record scratch, and the loaded prefix keeps its admitted frame/cell
storage. Inline state and temporary merge keys are charged to the worker stack.
One table descriptor is open in addition to the retained active-prefix descriptor;
CompleteReplay retains the verified table and borrows the prefix, releasing the
record scratch for reuse. Advance admits one table record plus bounded intervening
overlay work, and finish admits complete-table checks plus residual overlay work.
The allocation interval measures unchanged/deleted rows, input exhaustion and
completion, premature completion and sticky sink failure at both root bounds.
Full worker-stack/RSS and actual pin ownership remain integration obligations.

The supplied-byte frame_stream verifier retains a provider borrow, checked
header, digest, count/extent scalars and sticky error. Its compiled Provider layout is
capped at 512 bytes, charged to the worker stack. Each push borrows one exact
operation (at most 66572 bytes), which fits the existing 68 KiB record region;
no input bytes remain borrowed by the verifier after the call. The existing
allocation interval covers successful completion and sticky malformed input.
There is no new pool or frame arena. Future history/change cursor integration
must prove its collection and input scratch overlap within the admitted view;
this codec alone grants no completed cursor or new reservation.

The incremental journal-change entry point uses the existing journal verifier's
provider borrow, checked header, digest and scalar progress/error state; its compiled Provider
layout is capped at 512 bytes. Pending owns the existing Collector and borrows
that parent exclusively, with a compiled ceiling of 2 KiB. Both are charged
to the worker stack. Change slots remain in their separate 96 KiB reservation,
operation input uses the existing record region, and no journal/frame arena
is added. The allocation interval covers combined journal/change completion
and an abandoned frame. Runtime I/O scheduling and complete-worker stack/RSS
qualification remain unimplemented.

HistoryChangesInput retains separate CHANGE cells, one StoreReader, the
journal verifier, selection and compact progress/completion state. Each
advance borrows existing MAX_RECORD_BYTES operation scratch only for that
call; get/next can reuse it while a completed frame remains available.
Completed frames keep the original mutable slot capacity behind read-only
record access; consuming that result recovers all slots for the next frame.
Its compiled Provider layout is capped at 8 KiB, charged to the worker
stack, alongside the transient Pending and fixed 64-byte header/40-byte
footer. No input frame arena is retained. Opening uses shared private-path
scratch/transient parent handles. One frame admits at most 8258 explicit
read attempts, while its total bytes and operations retain the format
limits; callers still meter full-frame work and deadlines. The allocation
interval reuses a cold fixture buffer's record-sized prefix for selected
opening, local frame progress, completion and refusal at both root bounds;
that fixture allocation grants no new deployment reservation. Whole-worker
stack/RSS and live serving-view integration remain pending.

ActiveChangesInput uses the same private operation reader as HistoryChangesInput,
with PrefixReader fixing its captured extent and prefix-only completion. Its
compiled Provider layout also fits 8 KiB on the worker stack. Record scratch,
CHANGE slots, transient Pending and read allowances are unchanged; append growth
creates no new inventory. Existing root-bound allocation intervals cover empty
and populated prefixes, checksum completion and admission/premature-finish
refusals using the cold fixture record-buffer prefix. This is component evidence,
not complete-worker memory qualification or live-view activation.

The supplied-frame change Cursor fits 512 bytes on the worker stack. It owns
only ViewIdentity, kind/cursor, current Summary/index and terminal error state;
it borrows no operation buffer or CHANGE arena between calls. Each poll scans
at most the remaining 4096 compact slots in one frame and retains its next
index, so draining never rescans examined entries. NeedFrame performs no I/O;
the driver separately bounds initial location and each frame read. Existing
allocation instrumentation drains maximum changes, filters an empty result and
checks changed-view refusal without new hooks. No resource allowance changes.

ChangeRoute borrows its already admitted selected manifest and stores one
ViewIdentity, fitting a compiled 512-byte stack ceiling. A lookup examines at
most the format's 64 history descriptors, retaining no separate index or file
handles. Existing allocation instrumentation covers history/active routing and
below-floor/changed-view refusal. Physical frame location and complete-view
memory remain
separate work; this helper changes no arena or concurrency allowance.

ChangeInput holds one history/active reader plus captured identity, target and
fixed progress fields. The compiled Provider wrapper fits the existing 8 KiB
reader stack ceiling. Each advance retains the common 8258-read, one-frame bound
and borrows the same per-call record scratch. Finish reclaims original full
CHANGE capacity after selected completion, even after End or an empty prefix;
it does not clear or allocate a replacement arena. The returned completion
retains its descriptor until dropped, so the driver must budget retained handles
or drop it before moving segments. Existing root-bound allocation intervals
cover history-to-active scratch reuse, empty-prefix reclaim and target refusal.
Actual streaming view ownership and complete-worker measurements remain pending.

ChangeScan adds fixed cursor/source-transition state to one locator, borrowing
one slot arena throughout. Its compiled Provider layout remains within 8 KiB.
It drops a checked source completion before opening the next descriptor. Each
advance performs one open, bounded frame read, bounded slot drain or completion;
Progress changes no caller cursor. max_bytes is per source, and total scan work
still needs driver admission. The existing allocation interval covers a history
to active transition and successful full-capacity reclaim at both root bounds.

The account-verification allocation fixture loads actual CURRENT and checks all
selected metadata, replay, references, queue/mailbox scans and final blob files.
Four cold-created stores combine short/maximum root length with empty final data
and a published three-byte blob. Empty stores retain an incomplete active tail.
Each store has two measured intervals: success plus initial/mid-pipeline
deadlines, capture extent, overlay, history and read-work refusals, then a
separately prepared malformed CURRENT or exact blob-checksum failure.
All 16 snapshots must be valid; each before/after pair must match every counter,
including allocation, reallocation, deallocation, failed requests and live/peak
requested bytes. Scratch reuse and result disposal occur within measurement;
fixture creation, corruption writes, buffer allocation and cleanup are outside.
The existing positive controls and counter model run first. The --store-files
path also runs these cases. This qualifies representative composed Rust call
behaviour, not maximum datasets, native allocation, stack high water or RSS.

HistoryInput borrows one preallocated 1 MiB frame buffer and the selected
manifest, retains one StoreReader and the fixed journal verifier (header,
sequence/count/extent counters and provider digest state). Opening uses a
96-byte header buffer plus the existing transient handles and path scratch.
The selected extent must fit the admitted ceiling and the fixed 4 MiB frame
bytes plus header. Each frame shares 64 explicit read attempts across its
64-byte header and remainder; the header is checksummed before its length is
used. No whole-history inventory or additional per-frame buffer is allocated.
CompleteHistory retains one CompleteFile and Summary. Recovery/verification
must reuse an admitted existing frame arena (such as the writer's 1 MiB frame
buffer); this grants no additional per-view MiB. ReadView::next_change still
requires its separate cursor that skips PUT bodies with bounded I/O.
The allocation fixture creates
its 1 MiB buffer on the cold path before measurement and drops it afterward;
stream, completion, missing-descriptor/byte-cap and premature-finish refusal
run inside the interval at both root bounds. No new hook or service pool is
introduced.

PrefixReader retains one StoreReader with its extent fixed to the captured
prefix; CompletePrefix retains one File, Name, prefix length and LOCK borrow.
Opening uses the existing fixed path scratch and transient parent handles.
Sequential and completed random reads borrow caller slices and keep the
64 KiB step ceiling. No suffix buffer or per-prefix allocation is introduced.
The existing measured interval opens/consumes/completes a journal prefix,
checks bounded random reads and exercises invalid limits, short physical
extent, premature completion and sticky read failure at short/maximum roots.
Fixture files precede measurement; output uses a fixed stack array. This
qualifies primitive Rust allocations, not live read-view pinning or complete
service memory.

ActiveInput uses the same fixed journal frame reader as HistoryInput, with a
PrefixReader in place of StoreReader and captured sequence/offset counters.
It borrows an existing 1 MiB frame arena and Selection; complete active input
retains one CompletePrefix and Summary. Shared framing preserves the existing
64-call per-frame allowance and adds no dynamic dispatch or collection.
The measured interval reuses the history frame arena sequentially for empty
and populated active prefixes with an incomplete suffix, final binding and
invalid-view/admission refusal. Literal active files and metadata are prepared
before the snapshots. No extra per-view buffer or runtime pin pool is added.

RecoveryInput retains one whole-file StoreReader, fixed journal verifier and
valid sequence/operation counters and a fixed Current value, borrowing the
same admitted 1 MiB frame arena. It performs at most 64 explicit reads per
frame/tail, and no suffix allocation or directory inventory. ScannedJournal
retains the CompleteFile, verified Summary, selected Current and valid byte
boundary. The allocation interval reuses the active fixture with its
incomplete suffix and the existing frame arena, checks physical EOF
completion, whole-file byte refusal, premature completion and sticky read
failure at both root bounds. Fixture metadata preparation is cold; no worker
slot or extra frame reservation is introduced.

Explicit repair encodes and reads CURRENT using two fixed 120-byte buffers,
64 explicit extent reads and one EOF probe. After that input closes, it
retains the scanner's read-only File plus one private writable journal File;
opening uses shared fixed path scratch and transient parent handles. It calls
set_len and sync_all once, closes the writable File, then confirms the original
File's length and EOF. RepairedJournal retains one CompleteFile and Summary.
The allocation interval advances the fixture CURRENT to its supplied generation
using an already prepared CurrentUpdate, scans/repairs the active suffix,
checks bounded reads, rescans the repaired journal and refuses a second repair.
All metadata/intent preparation is cold; the 1 MiB frame arena is reused.
This fixture composes I/O primitives, not a validated complete store graph.

Pending/failed files retain their logical charges until explicit
cleanup, including when syncing consumed and closed their handles.

M04a1's `bounded.rs` supplies borrowed byte arenas, explicit-compaction wire
buffers and atomic text formatting. They neither allocate backing storage nor
grow it. Arena regions are disjoint Rust borrows; reuse requires their lifetimes
to end. Wire consume/clear and failed formatting do not erase old bytes.
Secret owners must enforce their own erasure/lifetime policy. Callers charge
copy/compaction/format work and provide storage from the ledger, rather than
requesting replacement storage when a helper reports capacity exhaustion.
Only bounded trusted Display implementations may be used on hot paths.
Reserve their maximum work before formatting, including failed attempts;
TextBuffer's visible byte length is not a formatter-transition counter.
Its byte view avoids revalidating UTF-8 when sending already encoded output.
Tests cover every small compaction overlap, direct-I/O count errors, exact
capacity, coexistence of arena regions and UTF-8/format rollback. Allocation
instrumentation and measured whole-process bounds remain M23 requirements.

M04a2's `ownership.rs` adds caller-owned fixed FIFO cells and slot bookkeeping.
Queue backing cells include Option layout; the final scheduler must measure
its actual entry types against the 32-byte reservation. SlotState uses at most
eight bytes and SlotId at most sixteen on tested targets. IDs have a process-
wide nonzero u64 generation, issued once and never reset when pools are dropped
or rebuilt. Ticket issuance tries one atomic compare/exchange: contention
returns a temporary error without reserving a slot, and exhaustion permanently
refuses issuance before wraparound. Generations do not authorize account data
and cannot be persisted or accepted from peers.

These primitives add no worker, locks or payload ownership enforcement.
M11's scheduler uses this lock protocol: briefly lock the pool and resolve the
token to locate its payload lock, then release the pool lock before acquiring
the payload lock. With that payload lock held, briefly acquire the pool lock
again and revalidate the complete token before accessing the payload. Reject a
stale token and release both locks without touching payload data. Never wait
for a payload lock while holding the pool lock. A live worker retains payload
ownership across its I/O, releasing the pool lock first. Release/reuse requires
the payload lock and then the pool lock; a main-thread deadline or cancellation
cannot free a worker-owned slot. Main uses try_lock and defers busy slots as
specified above. SlotPool::resolve is the only public index accessor, but its
returned integer cannot itself enforce this scheduler protocol.

Reserve completion credits before effects and never drop a durable result on
queue saturation. Releasing/dropping bookkeeping does not clear or cancel its
separately owned payload. Queue drop destroys pending values in FIFO order;
lifecycle code must drain admitted durable work before doing so. Tests check
FIFO saturation/wraparound, value ownership, stale/foreign and rebuilt-pool
rejection, concurrent unique issuance and finite exhaustion. Actual lock and
I/O scheduling race tests remain M11's implementation gate.

The unit cases exercise malformed/canonical IDs, unsupported configuration
versions, invalid pool relationships, arithmetic overflow, insufficient
memory, expansion beyond the default budget, and streaming quotas independent
of resident reservations. No claim about runtime allocation count, TLS or RSS
is made by these tests. Both host and sandbox cargo rosters discover the
standalone crate from its manifest; no manual crate list is needed.

## Diagnostic encoding bounds

M04d1's `observability::Event` holds fixed typed fields with no heap or borrowed
peer text. Its actual `Option<Event>` layout fits each planned 256-byte queue
cell; tests pin this ceiling. Event JSON Lines fit 1024 bytes. Explicit
authorized inspection borrows at most 256 UTF-8 source bytes and emits at most
4096 bytes, including worst-case escaping and its envelope. Events use the
existing 16 KiB log output partition; inspection uses 4 KiB of the separate
64 KiB administrative output region, never the log buffer or event queue.
Both append complete records atomically. These
encoders do not allocate the planned queue or implement the runtime log sink;
[OBSERVABILITY.md](OBSERVABILITY.md) defines their schema and disclosure limits.

M04d2 implements a caller-backed event queue of at most 384 cells. Construction
checks both the actual 256-byte cell ceiling and 96 KiB total before borrowing
storage. Queue loss accounting and fixed metadata fit the existing 16 KiB
rotation/drop/emergency partition. No sink or runtime synchronization exists
yet. Status JSON fits 4 KiB; the snapshot and at most 16 disk observations
fit 2 KiB. Layout/maximum-field tests pin both. The existing 16 KiB health
region contains two 4 KiB encoded slots, two 2 KiB observation cells, 2 KiB
emergency output and 2 KiB ownership/pin descriptors. Control receives exclusive
ownership of one inactive pair, encodes its complete line, and returns a checked
completion before main publishes it. Main never borrows the worker-owned pair;
health polling sends the previous immutable cache in at most 2 KiB chunks per
turn without waiting for ACME/DNS. Pinned slots cannot be overwritten, and no
third pair may be allocated. Slot saturation delays an update or refuses a
request. A missing/stale cache or failed worker uses the fixed empty-metric
`encode_unavailable` frame, which fits the 2 KiB main framing limit, in the
separate emergency output slot. M19 owns freshness, transfer/publication and
pinning; none is implemented by this encoder. Offline commands may encode into
administrative scratch. Counters saturate explicitly and missing observations
remain unknown. These are caller-owned helpers, not evidence of a running or
measured service.

## Rust allocation probe

M07e2a supplies a dedicated test executable with a System-forwarding Rust
GlobalAlloc counter, confined by UNSAFE.md T1. It counts allocation, zeroed
allocation, reallocation, deallocation and failed calls, requested live bytes
and lifetime peak bytes. Atomics are fixed static test overhead. Any arithmetic
failure invalidates the run. It does not track pointer identities; valid
GlobalAlloc callers supply matching layouts. It does not measure allocator
headers, fragmentation, transient realloc copies, direct native allocations,
RSS or stack bytes. Native/C measurements may overlap System traffic and must
never be added as disjoint totals without evidence.

All observations here run sequentially on the dedicated process's main thread.
Snapshots require quiescence; no counter is reset. A successful call's charge
is recorded before its pointer returns, and a free's charge is removed before
forwarding. The live/peak figures describe these accounting points, not an
instantaneous view inside System. Real forwarding controls must observe
alloc/zeroed/realloc/free/failure and an aligned allocation before accepting
zero-call evidence. Optimizations may remove allocations; this is evidence
for the actual pinned build and exercised paths, not proof for every source
path or compiler. Failure and overflow model tests are independent of the
process-wide counters.

The initial hot-path fixture repeats SHA-256 construction/update/finish and
SMTP line parsing with fragmented success, invalid framing, sticky refusal
and short-storage failure. It compares every counter before/after with no
logging inside the interval. It does not qualify TLS generations, sessions,
parallel workers, maximum inputs or whole-service memory, and it changes no
ledger allowance or service admission condition. Portable qualification runs
the fresh static executable and requires its exact completion record.

## Native allocation ownership table

M07e2b adds test-only, fixed-capacity address/size bookkeeping for the
C boundary probe. Entries contain integer addresses, never dereferenceable
borrowed pointers. Zero marks an empty slot and one remains a reserved invalid
address; real allocator results must be checked before insertion. Null
arguments to `free(NULL)` and `realloc(NULL, n)` must bypass removal. A zero-
size allocation with a nonnull address retains ownership until removal.
Deletion shifts displaced entries backward across the vacated slot while
preserving their lookup chains, including wraparound and a full table.

Each operation holds one atomic spin guard only while reading/updating fixed
entries and counters. No allocation, formatting, pointer access or callback
occurs under the guard. Snapshots acquire the same guard and are coherent
for table bookkeeping. Each lookup visits at most the table capacity; lock
contention itself has no elapsed-time or fairness bound. This test machinery
is not a service scheduling primitive or an async-signal-safe allocator. It
is also not fork-safe: a child could inherit another thread's locked guard.
The probe must not fork while tracking allocations.

Each deletion scans at most capacity minus one following entries and leaves
a real empty slot; no permanent tombstones accumulate with address churn.
Clustering and high occupancy can still require a full-capacity scan. No
constant-time or wall-clock bound is claimed.

Duplicate insertion, missing removal, reserved address, full capacity or
arithmetic failure leaves ownership unchanged and marks all evidence invalid.
The bit remains set while later successful operations can retire live entries.
Remove an old address before calling real free/realloc, with the guard already
released; a moved allocation can then safely race reuse of its former address.
A failed nonzero realloc must restore its old address/size. If another thread
occupied the vacant slot, restoration may fail with full capacity; this also
invalidates evidence without changing the real allocator result. Table bytes/peak
track bookkeeping instants, excluding storage inside the real allocator during
that call. The wrapper contract below handles zero-size resizes explicitly;
a generic null result does not establish ownership.

Host tests exercise collisions, closed-gap reuse, saturation, zero-size
ownership, arithmetic overflow, duplicate/unknown addresses, failed/moved
resize ownership and concurrent independent owners. Snapshots are coherent by
construction through the shared guard; the concurrent test is a smoke check,
not deterministic coverage of every interleaving. The Rust allocation probe
also exercises table insert/remove/snapshot in its no-call interval. Its
unchanged v1 success record intentionally includes these added table checks.
This table alone intercepts no native allocation, makes no libc coverage claim
and changes no runtime memory allowance. The C probe below reports fixed table
storage separately as instrumentation overhead.

The gap-closing tests include wrapped clusters, a slot-at-home that must not
move, complete return to empty slots under churn, and deterministic operation
traces checked against an independent ordered map at capacities 0, 1, 3, 4 and
7. They cover ownership/error/peak state as well as successful lookups.

Mixed traces choose operations from a high generator bit; their invalid flag
becomes sticky after the first error. Separate valid-only traces compare
200000 operations and require evidence to remain valid throughout. Immediate
error-path assertions pin duplicate and arithmetic-failure invalidation.

## Native allocator diagnostic executable

M07e2c adds a separate static musl test executable behind UNSAFE.md T2. Six
final-link wrappers preserve libc results, arguments, alignment and failure
behavior. A const, drop-free thread-local recursion guard makes nested calls
part of their outer operation; registry locks cover only bookkeeping and are
released before real allocator calls. The fixed 65536-slot table and counters
are instrumentation overhead, excluded from service memory evidence.

Null free/realloc arguments bypass removal. Nonnull zero-size results own an
entry with zero requested bytes. A failed nonzero realloc restores ownership;
a nonnull-pointer zero-size realloc is forwarded but invalidates evidence.
Overflow, table saturation, unknown ownership and failed TLS access also
retire the observation. No fork, signal-handler or asynchronous cancellation
use is qualified. Native snapshots are read only at quiescent control
boundaries. Four concurrent workers exercise forwarding and release; a
separate fresh process verifies that zero-size resize invalidates evidence.

The initial positive controls qualify forwarding and outer-call accounting;
provider RNG initialization requires a positive boundary call and keeps actual
native allocation paths in the link. Successful output separately reports
registry bytes, fixed counter bytes and per-thread flag bytes. TLS-block and
runtime thread overhead are outside those instrumentation figures.
Link wrapping covers undefined references only. Hidden/local libc calls,
startup paths and unqualified alternate allocators remain outside the counts.
Thus these are diagnostic C boundary observations, potentially overlapping
Rust System calls, not exact native heap or whole-service RSS measurements.
Service activation still requires the complete memory qualification in M07e.

Positive controls use opaque C function pointers and require exact nonzero
call deltas. Direct allocator calls used only in null checks can disappear
under compiler optimization; black-boxing the size arguments alone does not
establish that libc executed. These are checks of the pinned compiled
execution, never memory-safety assumptions about source allocation calls.

## Outbound lifecycle allocation observations

The dedicated Rust and native probes can each run the same socket-free
outbound scenario in a fresh process. Twelve ordered snapshots cover the
initial baseline, materialized configuration, first relay/ACME generation,
four allocated wire buffers plus handshake pool, reservation, native client
construction, a second published generation retaining the first through its
session, two pending sessions, capacity refusals, session release, 32 repeated
constructions using returned buffers, and final owner destruction. No server
material is opened and no network traffic occurs. Both roles use compiled
public roots. These are representative client paths, not maximum generations,
completed handshakes, established record traffic or service admission.

Each process first qualifies allocator forwarding with its existing positive
controls. It then stores observations in a fixed stack array and formats them
only after all measured owners are dropped. The supplied clock and
single-threaded scenario make snapshots quiescent. Reservation and saturation
must not change any counter; repeated warm construction must not retain
additional requested bytes. Cold provider initialization and its retained
state are included in observations, without assuming final totals return to
zero. The native controls' worker threads finish before measurement begins.

Rows start with `tls-rust` or `tls-native` and an ordered phase name. Rust
columns are cumulative alloc, zeroed, realloc, free, failed calls, requested
live bytes and lifetime peak. Native columns are cumulative malloc, calloc,
realloc, free, posix_memalign, aligned_alloc calls, live tracked blocks,
requested live bytes and lifetime peak. Peaks include controls and earlier
phases; subtracting snapshots does not produce a phase peak. Native counts
overlap Rust System allocations, so the separate runs cannot be summed.
Neither observation includes allocator overhead, all internal libc paths,
RSS, or stack storage. Instrumentation storage remains separately reported.

## Local handshake and record observations

Each allocation probe also has a fresh-process `--tls-handshake` mode. It
reuses the transport tests' synthetic P-256 certificate encoder, generates
private test keys, and compiles one complete SMTP/HTTPS/relay policy generation.
Only an ephemeral loopback socket pair is opened; configured production ports,
external services and certificate files are never opened. The default policies
must negotiate TLS 1.3, authenticate the relay's server name, leave the inbound
SMTP peer unauthenticated, and release both handshake permits upon completion.
One thread drives both endpoints with bounded progress loops and fixed time.

Thirteen snapshots cover baseline, generated material, resolved
configuration, compiled generation, four reserved buffers and socket setup,
constructed connections, handshake completion, one 16 KiB transfer in each
direction, 32 additional transfers in each direction, client release with
the server and all returned buffers retained, release of both connections
with all four buffers retained, explicit release of those buffers, and
complete owner destruction. Each transfer verifies all plaintext bytes.
Snapshot storage is fixed and output follows owner destruction. The columns
match the outbound lifecycle observations. Row prefixes are
`tls-rust-handshake` and `tls-native-handshake`. The builder accepts only the
ordered handshake schema and gives each process a distinct command log. The
handshake and large-chain completion records use allocation schema v2;
client, entropy and fragmented-input allocation records remain v1.

The repeated-to-client_released and client_released-to-released requested-byte
deltas show state released by each endpoint while shared generation/material
and wire reservations remain alive. They exclude fixed object/stack storage,
shared ownership and process-lifetime provider state; they are not complete
session costs. Releasing the four returned buffers must remove exactly
73748 requested bytes in each counter domain and four native tracked blocks.
This pins their allocation ownership independently of endpoint state. RSS
observes the same phases but does not require an equivalent resident decrease;
allocator retention and page accounting differ from requested-byte ownership.

Warm repeated records must retain no additional requested Rust bytes or tracked
C boundary bytes/blocks. This is a retention check, not a zero-call assertion:
TLS record operations can allocate and resize buffers. Requested lifetime peaks
include fixture material generation and prior controls. There is no per-phase
peak reset, and no attempt to sum overlapping counter domains. The fixture does
not cover TLS 1.2, mTLS, maximal certificate chains, maximum configuration,
concurrent workers, malicious peers, stalled sockets, process RSS or stack
ceilings. It neither changes the resource ledger nor activates the service.

## Entropy worker lifetime observations

Each counter domain also runs `--entropy-workers` in a fresh process after
its forwarding controls. Eight test workers request 240 KiB stacks, announce
startup, and wait without allocating. One worker initializes SystemEntropy
and fills 32 bytes; the remaining seven then do the same. All workers perform
64 further fills before waiting again. The observer requires every counter
to remain unchanged across those warmed fills. Native cold initialization
must increase malloc or calloc calls. Only after taking the repeated snapshot
does the main thread release and explicitly join every worker, including
thread-local destructors. A final snapshot follows scope teardown.

The seven phases are baseline, spawned, first_warm, all_warm, repeated, joined
and dropped. Fixed snapshot arrays are formatted after teardown; prefixes are
`tls-rust-entropy` and `tls-native-entropy`, with the same columns and lifetime
peak semantics as the TLS observations. Release/acquire checkpoints establish
that workers have finished each operation before the observer reads counters.
A drop guard releases waiting workers on a parent failure, checkpoint waits
have deadlines, and the portable runner enforces its process deadline.

These observations distinguish cold shared/worker costs and memory remaining
after worker exit without assigning each allocation to a provider subsystem.
They neither assert that process-lifetime provider state returns to baseline
nor include stack mappings, allocator overhead or all libc internal paths.
The requested test stack size is not measured stack/RSS evidence. Different
crypto operations, reseeding, maximum concurrency workloads and production
worker integration still require qualification. No ledger or service worker
count changes; Rust and C observations continue to overlap.

## Fragmented handshake refusal observations

The fresh-process `--tls-fragments` modes drive the socket-free crypto facade
with deliberately malformed ServerHello payloads. A reusable 16389-byte wire
buffer generates records directly, without retaining a second handshake copy.
With 16384-byte fragments, a 65511-byte payload reaches decoding and fails
Protocol; with 4096-byte fragments the corresponding payload is 65451 bytes.
Retained record headers consume the rest of the backend's 65535-byte limit.
For each shape, one extra payload byte must fail Capacity instead. These are
the existing reassembly boundary cases, observed through public APIs.

Twelve snapshots cover baseline, a public-root outbound policy, wire storage,
construction/pending input/refusal for each shape, both over-limit refusals,
32 repetitions of all four cases, and complete teardown. The pending snapshot
precedes the final record; it is not a snapshot at the exact transient peak.
The `large_refused` and `small_refused` snapshots follow explicit session drop;
they cannot distinguish release at refusal from release during drop. Refusal
must retire handshake evidence, and the fixture then drops the session.
Repeated cycles must retain no additional requested Rust bytes or tracked
C bytes/blocks. Counter
rows use `tls-rust-fragment` and `tls-native-fragment` with the established
columns. Lifetime peaks still include controls and policy setup.

This fixture generates no valid peer certificate chain, successful handshake
or established traffic. It does not add mail pump buffers, service admission,
concurrent handshake slots or whole-process RSS accounting. It measures a
specific untrusted-input allocation path, not every adversarial handshake or
a total session bound. Provider state can survive session teardown; the two
counter domains remain overlapping observations.

## Large local certificate-chain observations

The `--tls-large-chain` modes reuse the local TLS 1.3 handshake/record scenario
with a synthetic leaf, intermediate and root. Unique issuer/subject names bind
the signed chain. Each certificate carries 15000 bytes in an unknown
noncritical extension and must remain within the facade's 16 KiB DER ceiling.
The complete PEM chain must exceed 60 KiB and fit the loader's 64 KiB input
ceiling; this tests a large admitted chain, not every maximum encoding.
Ordinary fixtures retain their original certificate shape without padding.

The thirteen phases and columns match the ordinary handshake scenario, with
`tls-rust-large-chain` and `tls-native-large-chain` prefixes. Require
authenticated server name, TLS 1.3, returned handshake permits, verified
bidirectional 16 KiB
traffic and no additional retained bytes/blocks across repeated records.
Fixture material, its source PEM bytes and both endpoints remain charged in
these observations; they are not isolated per-session or per-generation costs.
Lifetime peaks include earlier controls. No ledger limit, production trust
policy, generation admission or whole-service RSS claim changes.

## Sampled process RSS observations

A separate unwrapped `rss_probe` integration executable reuses the shared
allocation scenarios. Each mode runs in a fresh process without either
allocation counter or the native registry. An observer opened before the
scenario rewinds `/proc/self/smaps_rollup` into a fixed 4096-byte buffer and
requires one complete read followed by EOF. It extracts exactly one positive
`Rss:` value with kernel `kB` units (KiB); duplicate, missing, malformed or
truncated values fail. Unsupported procfs is an error, with no fallback.
The Linux [procfs documentation](https://docs.kernel.org/filesystems/proc.html)
describes the smaps interfaces for more accurate resident accounting than the
scalable, approximate status/statm counters.

A separate default process checks parsing refusals and touches a 16 MiB
allocation, requiring at least an 8 MiB resident increase. It reports before,
live and dropped samples without requiring allocator release after drop.
The large margin is a positive observation control, not an exact page-count
claim. Those controls run separately so they do not warm scenario processes.

The existing phase names are retained. Rows have `rss SCENARIO PHASE KIB`,
where SCENARIO is client, handshake, entropy, fragment, certificate-list,
large-chain, generation,
generation-routing, generation-trust, remote12, remote13 or remote13large.
The completion record is `rss-observation-v2: SCENARIO passed`. Samples use
fixed arrays and are
formatted after scenario teardown. Positive controls use the control scenario
with baseline, touched and dropped phases.

RSS includes resident code, stacks, shared pages, allocator overhead, fixture
material and the observer itself. It is neither a disjoint native-allocation
count nor a per-session charge. A rollup is a kernel walk, not an atomic
snapshot of concurrently running threads. Checkpoints reduce fixture activity
but do not turn these samples into transient high-water marks. Page residency
and allocator retention vary by host and run. Whole-service worst-case RSS,
maximum concurrency, stack coverage and ledger qualification remain pending;
these observations change no memory allowance or service activation rule.

## Sixteen-profile generation observations

The `--tls-generations` mode in each counter executable and the unwrapped RSS
executable constructs sixteen independently admitted files identities from
synthetic large chain/key material. A direct SMTP listener and one HTTPS
listener consume two profiles; fourteen MTA-STS domain names select the other
profiles through the HTTPS configuration. The complete table has three roles,
including explicit relay trust. Every preparation must read sixteen chains,
sixteen keys and one relay CA. Each chain exceeds 60 KiB of PEM and remains
within the 64 KiB local input ceiling; each certificate remains within 16 KiB
DER. Reusing fixture bytes does not bypass per-profile admission. The test
creates no service listener, ACME request or network connection.

The additional `--tls-generation-routing` mode retains sixteen distinct large
chains and covers sixteen listeners (one direct SMTP and fifteen HTTPS) with
256 MTA-STS domains. Of these, 255 use the maximum admitted 243-byte domain
length and derive 251-byte certificate names; one short existing domain and
localhost bring the distinct per-profile bindings to 257. Distribute the
bindings across sixteen profiles, preserving the 32-name per-profile ceiling.
Each HTTPS table selects all sixteen profiles. All listeners use private
loopback addresses as configuration data; none binds a socket. The compiled
table must contain seventeen policy entries, including relay, and read the same
sixteen-chain/sixteen-key/one-CA inventory. The original mode remains a
separate observation.

Eleven ordered checkpoints record baseline, retained material/configuration
text, materialized configuration, first published generation, replacement
candidate, publication with the old lease retained, third-generation
refusal, old-lease release, four further replacements, current-generation
release and complete scope teardown. The capacity refusal must leave both
counter snapshots unchanged. Releasing the old lease must return live
requested bytes to the first-generation level; repeated replacements must
preserve that level. Native tracking also checks live blocks at both
boundaries. The raw material, configuration and clock remain alive at the
current-generation-release checkpoint. Fixed arrays hold observations until
all owners drop. Allocation records use the distinct
`tls-generation-allocation-v1: DOMAIN passed` completion; RSS uses its
existing v2 completion with the generation scenario. The routing mode
instead uses `tls-generation-routing-allocation-v1: DOMAIN passed` and the
`generation-routing` RSS scenario. Both use the same checkpoints and
oracles; the parser rejects crossed scenario names as well as missing,
duplicate, reordered or wrong-version records.

These requested-byte differences include the independently compiled identities
and shared HTTPS routing objects, but exclude fixed caller state, stack and
allocator overhead. Their lifetime peaks include fixture preparation and both
generations. The domains overlap. RSS includes the entire fixture and observer
and need not fall on release. The routing case reaches the schema's profile,
listener and domain counts with long derived names. It does not maximize every
profile's name count, certificate-name length, gateway policies, trust bundles
or mixed key algorithms. A configuration with explicit MX on every domain can
use sixteen HTTPS listeners without direct SMTP; this case has only fifteen
HTTPS tables. The allocation probes additionally require this routing case's
first-to-candidate requested-byte increase to fit a narrower 1 MiB regression
guard. Shared HTTPS identity views avoid retaining another copy of the same
names for every listener. This fixture-specific check excludes allocator
metadata. The allocation case uses one common JMAP-primary profile; it does
not measure all possible primary-role combinations or prove an aggregate upper
bound for all configurations;
complete concurrent session and generation qualification remains M07e.

All three generation allocation modes check the first retained generation and
the first-to-candidate retained delta against `TLS_GENERATION_BYTES`. The
first construction peak minus pre-compilation retention must fit one entry.
The candidate and final replacement peaks minus the first retained generation
snapshot must each fit one entry, so construction cannot borrow the old
generation's unused allowance. The final replacement peak minus the
pre-compilation retained snapshot must also fit two entries. These lifetime
peaks conservatively include earlier construction; they do not isolate
attribution or prove every configuration fits.

## Gateway trust generation observations

The existing diagnostic artifacts also accept `--tls-generation-trust`.
This cold configuration case retains sixteen admitted large certificate
profiles, fifteen gateway listeners and one HTTPS listener. Each gateway
references its own declared policy and the same synthetic private CA bundle
contents. The relay also uses that bundle. It contains 128 distinct anchors
with different bounded subjects, one P-256 key and noncritical padding; its
PEM size is greater than 112 KiB and at most the admitted 128 KiB. Every load
parses the complete bundle. No peer handshake or successful authentication is
claimed by this configuration-only fixture.

The fixture explicitly declares fifteen SMTP slots and a 128 MiB planner
budget so the fifteen one-slot gateway listeners are admitted. It supplies an
external MX for every domain, with no direct SMTP listener. These settings
belong to the fixture. Assertions check the decoded profile/listener/gateway
counts, material-open counts and complete seventeen-entry policy table.

Rust/native allocation processes enforce the same unchanged third-slot
refusal, old-generation release and four stable replacements as the other
generation cases. RSS is sampled in its own process. Exact
`generation-trust` schema labels distinguish all eleven lifecycle phases,
with allocation completion `tls-generation-trust-allocation-v1: DOMAIN passed`
and RSS completion `rss-observation-v2: generation-trust passed`. Other
scenario output cannot supply its evidence. The first-to-candidate retained
delta is about 1.2 MiB in both counter domains and must fit the generation
entry. Its single HTTPS listener has no views to share across listeners.
Complete aggregate qualification remains required before service activation.
The case does not establish a maximum for arbitrary subjects, mixed algorithms,
ACME trust or concurrent sessions.

## Large remote-chain observations

The unwrapped `tls_memory_process_tests::remote_chain_observations` controller
creates four signed synthetic certificates, each within 16 KiB DER and totaling
more than 63 KiB but at most 65000 bytes. A private td-crypto process peer loads
those DER files directly; this remote fixture can exceed the local 64 KiB PEM
input ceiling without changing that ceiling. It restricts each connection to
TLS 1.2 or TLS 1.3. Its private TLS 1.3 ticketer emits two opaque 16000-byte
test payloads and counts both emissions. The additional
`remote_large_ticket_observations` controller case emits one 65000-byte ticket
under TLS 1.3, identified as remote13large. Neither mode performs encryption
or accepts resumption. Keys, peer allocations and controller work are outside
the observed process. All sockets are loopback, all material is private
temporary test data, and children have bounded waits with owned cleanup.
Instrumented processes never spawn children. Cleanup runs on normal return and
unwinding; an outer hard kill can leave children and temporary material until
runtime namespace cleanup.

For each version/ticket mode, the runner starts three fresh controllers. Each
controller starts one Rust, native or unwrapped RSS observer with an explicit
peer address and root. Eleven snapshots cover baseline, loaded
configuration/root, client generation, reserved wire buffers/socket, session
construction, authenticated handshake, one echoed 16 KiB record, thirty-two
further verified echoes, client release, returned-buffer release and final
owner teardown. Both counter domains require stable requested bytes across
repeated records; native also checks blocks. Dropping the returned pair must
release exactly 36874 requested bytes and two native blocks. The handshake
must report the requested version and server-name verification. Record
processing includes any queued tickets; it does not isolate their transient
peak. Fixture configuration and trust remain alive through client and buffer
release.

The portable runner requires a successful controller test, unique observation
boundaries, exact v2 domain/scenario completion and every ordered inner row
before printing measurements. Allocation records use distinct remote12,
remote13 and remote13large v1 schemas; RSS uses v2. Earlier controller
completion versions are refused. Each controller has a separate bounded output
log and thirty-second outer deadline. These are test-only environment
channels, not service configuration or a public backend interface.

The repeated-to-released delta exposes client-owned requested heap; adding the
separately released wire pair counts each once. It still excludes shared
configuration, fixed objects, stacks and allocator overhead. The allocation
probes require the record checkpoint's retained bytes minus the retained
generation checkpoint to fit `TLS_SESSION_BYTES`. This includes the wire pair
and conservatively charges native caches initialized during session work.
The repeated checkpoint's lifetime peak minus that generation baseline must
fit the session entry plus the larger handshake/established-processing entry.
A separate guard subtracts the retained authenticated-handshake snapshot
from the repeated-record lifetime peak and checks only the established-
processing entry. Earlier construction/handshake peaks are included, not
isolated. These are guards on the measured cases, not worst-case accounting
over all peer inputs. Retained bytes, lifetime high-water observations and
sampled RSS remain distinct, and the
Rust/native domains overlap. This fixture is not maximum simultaneous queued
traffic, maximum tickets, mTLS or complete service qualification. The
large-ticket case covers one near-ceiling fragmented ticket. Socket-free
crypto fixtures also qualify selected accepted flights and terminal refusal
of two 48000-byte tickets in one flight; they do not establish an exact
threshold or refusal-memory peak. The known session-admission blocker remains
explicit; configured queue limits are not a whole-memory bound.

## Decoded certificate-list refusal observations

The existing diagnostic artifacts accept `--tls-certificate-list` in
separate fresh Rust/native/RSS processes. This untrusted outbound-client
case sends an unexpected plaintext Certificate message in the TLS 1.2 format
before ServerHello, carrying 21800 empty DER entries. Its three-byte list
length plus entries form a 65403-byte body; records fragment it at 16 KiB
and 4 KiB. All wire storage is allocated before driving the sessions. The
pinned backend constructs its entry vector before protocol-state validation
refuses the message. This measures rejection of malformed traffic, never
peer authentication.

Eleven phases cover baseline, retained client policy, wire storage, each
fragmentation case's constructed/pending/refused states, 32 repetitions of
both cases and final teardown. Rows use `tls-DOMAIN-certificate-list` with
completion `tls-certificate-list-allocation-v1: DOMAIN passed`, or the
independent RSS schema. Both allocation probes require at least 512 KiB of
additional lifetime requested-byte peak above the first pending peak
snapshot, proving this pin exercises decoded-entry allocation. This is a
qualification control, not a required allocation floor for future backends:
it relies on this pin's vector growth, including spare capacity. After the
second refusal, all repetitions must leave retained requested bytes
unchanged; native live block counts must also stay fixed. Refusal consumes
the session, discards queued output and refuses later reads, writes and
records.

The final repeated-refusal lifetime peak minus retained client policy must
also fit the session plus handshake entries. This covers both fragmentations
and all repetitions, conservatively including wire storage and prior peaks;
it does not qualify post-Finished decoding against the separate established-
processing entry.

The input is bounded but decoded storage can be much larger than its wire
body. Counter peaks do not include allocator metadata or every realloc
transient; Rust/native domains overlap. RSS samples occur between calls and
can miss allocations already freed by a refused call. This single TLS 1.2
pre-ServerHello case does not bound encrypted TLS 1.3 certificate lists,
post-handshake messages, concurrent sessions or total service memory. M07e
retains the aggregate admission requirement.
