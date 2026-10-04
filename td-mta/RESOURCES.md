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
