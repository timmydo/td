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
  not a claim that the host's virtual stack mapping is one MiB.
- TLS sessions include SMTP, HTTPS and outgoing delivery slots; handshake
  scratch is additional. The handshake cap is global in this profile.
  HTTP-01/administration must use the existing fixed control/I/O reservations;
  they cannot silently add another general connection pool.
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
| Log, 128 KiB | 96 KiB queued fixed events; 16 KiB encoder/output; 16 KiB rotation/drop counters and emergency status |
| Cold reload, 2 MiB | Two immutable configuration snapshots of at most 1 MiB each, including referenced credential data; reject a third live generation |

The resolver's 128 KiB includes 128 cache entries of at most 384 bytes
(48 KiB), a 64 KiB packet/TCP buffer and 16 KiB question/name/alias-chain,
address-result and cursor state. Cache keys are configured endpoint index plus
configuration generation, not copied DNS names/CNAME chains. Entries retain
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
records with 128-byte entries (8 KiB), and a remaining 72 KiB for timers,
slot generations, bounded generation pins, queue heads and counters. A
reservation record stores references/charges; it does not embed a frame.
The window can be refilled from the disk due-time index, never from a full
in-memory queue. Scratch/queue bounds apply even with configured larger pools.

Each 1 MiB configuration snapshot permits a 512 KiB text/secret arena,
4096 alias descriptors of at most 32 bytes (128 KiB), and 384 KiB for domain,
identity/device/endpoint descriptors, resource plans, indices and ownership
metadata. Combined arena bytes still bound configurations with many long
values. Build a new snapshot from the bounded stream scratch; do not keep an
extra file-sized input copy beside both snapshots. Certificate/key provider
allocations belong to their separate TLS/certificate entries. Pin old snapshots
only for bounded operation lifetimes and reauthorize Access as API.md specifies.
Exact stanza/field limits and snapshot structs are M04's implementation gate.

This ledger does not budget whole earlier JMAP responses, generic JSON trees,
or all MIME body values in memory. ADMISSION.md defines bounded private response
spools/result references and their work/disk admission. POLICY.md defines
parser/charset/search/thread policies and CASES.md the fixture inventory.
Concrete implementations must fit these contracts before enabling service.

## Evidence

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

The unit cases exercise malformed/canonical IDs, unsupported configuration
versions, invalid pool relationships, arithmetic overflow, insufficient
memory, expansion beyond the default budget, and streaming quotas independent
of resident reservations. No claim about runtime allocation count, TLS or RSS
is made by these tests. Both host and sandbox cargo rosters discover the
standalone crate from its manifest; no manual crate list is needed.
