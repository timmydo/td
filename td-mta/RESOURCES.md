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
| Storage read views | 2 | 4587520 | 9175040 |
| Writer and checkpoint | 1 | 6946816 | 6946816 |
| Resident index cache | 1 | 8388608 | 8388608 |
| Outbound slots | 1 | 572672 | 572672 |
| Sort runs and merge buffers | 1 | 1048576 | 1048576 |
| Log queue and formatting | 1 | 131072 | 131072 |
| DNS, ACME and control scratch | 1 | 524288 | 524288 |
| Slot queues and queue window | 1 | 131072 | 131072 |
| Fixed worker stacks | 8 | 262144 | 2097152 |
| Main stack allowance | 1 | 1048576 | 1048576 |
| TLS session headroom | 17 | 131072 | 2228224 |
| TLS handshake headroom | 2 | 1048576 | 2097152 |
| Certificate generations | 2 | 1048576 | 2097152 |
| Cold reload overlap | 1 | 2097152 | 2097152 |
| Process and allocator allowance | 1 | 8388608 | 8388608 |
| **Total** | | | **64464128** |

The total is approximately 61.48 MiB against a 64 MiB configured budget;
the remaining 2644736 bytes are unassigned headroom, not another cache.
The 128 MiB workload RSS release ceiling remains independent. Raising the
configured memory budget does not preserve the default RSS claim.

## Slot composition and ownership

- SMTP: bounded headers, 320 bytes per envelope recipient, 64 KiB I/O and
  16 KiB state. The fixed recipient cell must hold the SMTP path plus metadata.
- HTTPS: request bytes, 16 bytes per JSON token, 128 bytes per largest
  get/set/query result window entry, and 96 KiB framing/output scratch.
  Escaped strings are streamed; request tokens borrow the request arena.
  Earlier method results and created-ID maps use the bounded disk retention
  contract in ADMISSION.md, never extra per-method heap trees. Event streams use slots
  without pinning storage views between emissions.
- Body job: headers, 64 bytes per MIME descriptor, 96 KiB decode/work scratch.
  Nested parsing and transfer decoding share that reservation.
- Read view: 4 MiB journal prefix, 32 bytes per journal operation, 128 KiB
  cursor/value scratch. Backup consumes an existing view.
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
  These count against the existing 128 KiB session target, not extra headroom.
  Before the final handshake flight drains, the facade permits 83973
  ciphertext bytes. Its conservative sum with 16384 plaintext bytes and
  36874 pump bytes remains 137231, exceeding the 128 KiB session target by
  6159 bytes. This window can extend past authenticated Finished. The mail
  owner retains its handshake permit until facade and socket output drain;
  the corresponding additional handshake memory reservation must cover the
  whole pre-drain window before activation. After authenticated Finished and
  complete facade output drain, its permanent ciphertext ceiling is 36874 bytes. That established
  ceiling plus 16384 plaintext bytes and the pump buffers totals 90132 bytes,
  leaving 40940 of the 128 KiB session target before handles, retained input,
  peer chains, native state and allocator overhead. Post-operation output
  refusal does not bound temporary allocation or Vec capacity. Complete
  session accounting remains an M07e admission blocker. Measure coexistence
  within 128 KiB, or amend the ledger before serving. The runtime must reserve
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
  HTTP-01/administration must use the existing fixed control/I/O reservations;
  they cannot silently add another general connection pool.
- Each 1 MiB certificate generation includes every server profile and parsed
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
  does not yet measure/enforce the 1 MiB native aggregate and cannot activate
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
retained). ADMISSION.md freezes free-space/metadata/inode quotas, completion
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
| Read cursor/value, 128 KiB/view | 64 KiB value; 1 KiB key; 63 KiB cursors, history streaming, sparse-index lookups and checksums |
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
records with 128-byte entries (8 KiB), sixteen filesystem records including
slot generations at no more than 128 bytes each (2 KiB), one dedicated
checkpoint-attempt record plus sequence counter (at most 512 bytes), and a
remaining 71,168 bytes for timers, other pools' slot generations, bounded
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
raw-body quota and actual free capacity for each operation. These settings do
not guarantee that any particular upload/submission can currently fit.

M04c2's admission work/timer helpers charge fixed scalar counters and compute
checked deadline budgets without allocation. Runtime callers own clock
sampling, fixed scheduling steps, protocol transitions, nested-budget charging
and completion of admitted durable work. These helpers do not establish
whole-process memory usage or execution-time enforcement.

M04c3a's space evaluator uses scalar counters and injected probe samples. It
protects pending and checkpoint capacity in its arithmetic, including writes
completed during a probe. M04c3b1 supplies fixed logical reservation records,
atomic grouped quota checks and linear effect tickets. M04c3b3b adds a physical
filesystem binding and remaining rounded growth to the same cell. The combined
cell plus slot fits 128 bytes, enforced by a layout test; no second reservation
table is allocated. No replacement cells
are allocated on saturation. M04c3b2's scalar writer ledger wraps this same
table; each framed job uses one of its cells for journal bytes/operations.
Prepared requests and quota projections use fixed stack arrays, with no heap
growth. Its checkpoint barrier preserves pending frame ownership and does
not reopen on selection. M04c3b3b couples quota and filesystem changes using
a fixed sixteen-entry stack projection and at most eight staged lease records.
It consumes/rechecks probes at admission and publishes prevalidated filesystem
changes only after logical installation succeeds. M04c3b3c2 transfers protected
capacity to one dedicated checkpoint attempt, so a full client lease table
cannot starve its reservation. Building quota stays in the same Quotas ledger.
Attempt and I/O tokens live in owned job scratch; dropping one pins admission
or its effect until reconciliation. Transfers and completions use fixed stack
projections. M05/M08 own actual
written/orphan cleanup, writer/view locking and publication authority.

M04c3b3a's filesystem table borrows its backing cells and SlotStates from
that 2 KiB partition; saturation refuses without allocation. Probe tickets
and observations live in caller-owned job scratch, not the 32-byte queue
entry itself. Queue entries carry references to owned job payloads. Matching
an observation grants no capacity. The composed coordinator tests drive
physical counter transitions using injected samples and completion proofs.
They establish accounting bounds, not runtime filesystem behavior or RSS.
M04c3b3c1 stores one probe epoch on the registry and copies it into the fixed
stack projection. The filesystem record/slot size is unchanged, and no per-probe
table is allocated. Invalidation retains capacity and lease ownership.

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
first-to-candidate requested-byte increase to fit the planned 1 MiB generation
entry. Shared HTTPS identity views avoid retaining another copy of the same
names for every listener. This fixture-specific check excludes allocator
metadata. The allocation case uses one common JMAP-primary profile; it does
not measure all possible primary-role combinations or prove an aggregate upper
bound for all configurations;
complete concurrent session and generation qualification remains M07e.

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

The fixture explicitly declares fifteen SMTP slots and an 80 MiB planner
budget so the fifteen one-slot gateway listeners are admitted. It supplies an
external MX for every domain, with no direct SMTP listener. These settings
belong to the fixture; shipped defaults and the 1 MiB generation ledger entry
are unchanged. Assertions check the decoded profile/listener/gateway counts,
material-open counts and complete seventeen-entry policy table.

Rust/native allocation processes enforce the same unchanged third-slot
refusal, old-generation release and four stable replacements as the other
generation cases. RSS is sampled in its own process. Exact
`generation-trust` schema labels distinguish all eleven lifecycle phases,
with allocation completion `tls-generation-trust-allocation-v1: DOMAIN passed`
and RSS completion `rss-observation-v2: generation-trust passed`. Other
scenario output cannot supply its evidence. Host observations exceed the
planned 1 MiB generation allowance. Its single HTTPS listener has no views to
share across listeners. Reducing retained
trust data, revising the allowance or narrowing admitted configuration remains
required before service activation. The case does not establish a maximum for
arbitrary subjects, mixed algorithms, ACME trust or concurrent sessions.

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
configuration, fixed objects, stacks and allocator overhead. In host and
qualified musl runs this subtotal already exceeds the 128 KiB session target.
Complete worst-case accounting must precede a ledger revision. Retained bytes,
lifetime high-water observations and sampled RSS remain distinct, and the
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

The input is bounded but decoded storage can be much larger than its wire
body. Counter peaks do not include allocator metadata or every realloc
transient; Rust/native domains overlap. RSS samples occur between calls and
can miss allocations already freed by a refused call. This single TLS 1.2
pre-ServerHello case does not bound encrypted TLS 1.3 certificate lists,
post-handshake messages, concurrent sessions or total service memory. M07e
retains the aggregate admission requirement and the current ledger
unchanged.
