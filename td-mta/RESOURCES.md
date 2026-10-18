# Resource planning and component evidence

[DESIGN.md section 5](DESIGN.md#5-resource-and-execution-model) owns resource
policy. This document inventories current code, a proposed runtime layout and
fixed-fixture observations. The byte ledger, scratch partitions, worker layout
and provider allowances below are planning estimates. They do not impose
byte-exact architectural ceilings, blanket preallocation/zero-allocation, or
qualification of every listed exclusion before functional implementation.
Only explicitly implemented caps and API capacity/ownership rules describe
current enforced behavior. Historical milestone observations remain scoped
to their fixtures. New evidence follows section 5's stopping rule; whole-service
performance is evaluated after integration in M23.


M01 and M02c3a implement `Limits::plan` in [src/limits.rs](src/limits.rs).
It validates
counts, individual ceilings, relationships and checked arithmetic before
returning a fixed-size ledger. It allocates no pools and starts no workers.
`ConfigVersion` versions operator configuration, not the unimplemented disk
format. Local IDs are distinct 16-byte types with canonical lowercase hex;
they convey no authorization. Derived MIME-part wire IDs belong to M02.

## Default reservation

All quantities below are bytes. This is a planning estimate for each
component, not a claim that the service exists or its RSS has been measured.
Ownership below records proposed structures/workers; M04 implements bounded
primitives and M07 measures TLS components. M23 measures integrated resident
usage. A structure need not fit these estimates as a design requirement.
Limits::plan still enforces its existing arithmetic/configuration budget;
changing that validator requires an explicit atomic implementation update.

| Component | Count | Bytes each | Total |
| --- | ---: | ---: | ---: |
| SMTP slots | 8 | 376064 | 3008512 |
| HTTPS slots | 8 | 1703936 | 13631488 |
| Body/search jobs | 2 | 1310720 | 2621440 |
| Storage read views | 2 | 299008 | 598016 |
| Resident index cache | 1 | 4194304 | 4194304 |
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
| **Total** | | | **78365952** |

The total is approximately 74.74 MiB against a 128 MiB configured budget;
the remaining 55851776 bytes are unassigned headroom, not another cache.
The default process ledger excludes database-server memory and WAL mappings.
The two default TLS handshakes remain configurable within the process budget.
The 64 MiB idle and 128 MiB workload RSS figures are provisional goals under
DESIGN section 5. TLS entries estimate demand, not additional arenas to
allocate and touch at startup. The ledger sum establishes no RSS bound.
Provider counters observe requested bytes in overlapping domains; allocator
overhead and realloc transients require care when interpreting measurements.
M07/M23 use those observations to diagnose actual resource problems, not to
prove that every admitted input fits each speculative entry.

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
  Keys and string values are unescaped in place before request tokens borrow
  the request arena. Unescaping cannot expand the UTF-8 byte length; the
  shared JSON arena parser validates escapes and updates token extents
  before immutable borrowing. Output re-escapes strings into the bounded
  sink. No separate decoded-string arena is owned. The shared arena Cursor
  keeps at most 4 KiB fixed state within the 40 KiB
  argument/scheduler/framing partition of HTTPS scratch and advances
  at most 256 transitions per step, each inspecting at most 12 input bytes
  or comparing one key byte. UTF-8 validation, tokenization and linked key
  sorting all yield; finish exposes only a completed document. The convenience
  parse helper drives the same cursor synchronously. HTTPS remains inert:
  the Validation owner now drives stepped parsing and whole-envelope/I-JSON
  validation with at most 256 scalar/capability transitions or sixteen
  structural transitions per turn. Scalar/comparison batches stop at their
  phase boundary. Creation IDs use eight-byte chunks; each method tuple
  has exactly three items. Its
  fixed state, including the parser cursor, fits 8 KiB of the same scratch.
  Activation must check live admission between turns and avoid unbounded
  scalar-view, object-lookup and serialization helpers on worker turns.
  The shared OwnedCursor can move preallocated boxed request/token arenas
  between worker turns and returns those same arenas after completion,
  cancellation or refusal. Its completed owner lends immutable document
  views. It adds no arena copy, allocation or parser state allowance;
  the occupied prefix bounds parsing even when capacity is larger.
  JMAP's OwnedValidation owns the completed parser document and uses the
  same envelope machine as borrowed Validation. Its resumable state contains
  only positions into that document, counts and phase tags, never borrowed
  values. Each turn lends views for only that turn; completed OwnedRequest
  lends the full envelope without parsing or validation again. Refusal and
  cancellation return the same arenas. Both validation forms fit 8 KiB.
  OwnedValidation checks the document's recorded parser ceilings against
  its ResourcePlan, checks token capacity and derives the method limit from
  that same plan. Runtime retains the original work/deadline admission
  throughout all phases.
  Shared OwnedEmitter supplies bounded value serialization in at most
  8 KiB of the 16 KiB spool-walker/escaping partition. Its existing document
  boxes remain the request/token arenas, not a second copy. Its caller's
  output uses the separate 16 KiB JSON-output partition. Each turn has at
  most 256 transitions, each copying at most 256 number bytes, five literal
  bytes, or at most 32 shared string-frame scalar/escape steps. Exact counting uses
  that same encoder. Its 128 frames cover parser-produced request values;
  they are not ADMISSION.md's separate 160-frame retained-response walker.
  The partition leaves 8 KiB for that walker's future fixed state. Both may
  coexist only if their combined state fits 16 KiB; runtime must prove this
  before activation, without an additional scratch allocation.
  Runtime must retain document-bound request indices,
  enforce live admission, charge retention and publish only complete values.
  This serializer is not Core dispatch, response reservation or a release
  gate for capabilities; scalar-view and lookup helpers remain synchronous.
  Whole-buffer bounded allocation alone is not scheduler qualification.
  The inert mandatory-response Preflight and fallback writer reuse that same
  owned document and encoder. A turn writes at most 16 KiB into caller scratch
  and performs one literal/structural transition, one OwnedEmitter poll, or
  at most 256 shared string-frame steps for the captured Session state. No
  string, response tree or arena copy is allocated. Counting output is private
  scratch, not a retained response. The passive plan includes exact wire
  framing/IDs/state/map bytes, a second seed-map copy and the bounded error
  allowance specified in ADMISSION.md. The enclosing dispatcher still charges
  live work/time and reserves disk/index/method capacity; no endpoint uses this
  planner yet. Tests pin Send and a maximum 8 KiB state size for these owners,
  within the existing writer estimate; any combined retained-response walker
  still needs its integration measurement. Constructor/consuming refusal drops
  the request allocations. Cancellation or step refusal can return the same
  arenas through into_request instead; this is distinct from the earlier
  validation owners' recoverable consuming refusals.
  The inert result index owns the same request arenas and 128 fixed response
  entries; a size test checks ADMISSION.md's 8 KiB response-index planning estimate. Append visits
  at most 64 call tuples without traversing argument trees or string bytes.
  Lookup compares one primary ID, at most 256 call-ID bytes plus a 64-byte
  server name per turn, and may skip one implicit entry by token identity.
  Across R references, worst-case comparison work scales with R times the
  sum of earlier call-ID lengths; identical long prefixes can exhaust the
  original runtime work budget, which must report a resource failure. Reference strings and the immutable index
  stay borrowed until lookup ends; no string copies or response tree are
  allocated. This is not file custody or an authorization to read a Span.
  Runtime must bind the index to retention and charge work/deadline checks;
  combined-owner memory and actual disk traversal still need qualification.
  The shared retained-JSON Scanner has 160 fixed structural frames and no
  retained string/value allocations. Its test pins scanner state to at most
  512 bytes, leaving the rest of the walker estimate for integration; combined owners still need measurement.
  It consumes at most 4096 source bytes and emits at most one structure/key
  event per poll, preserving lexical progress across arbitrary read splits.
  Source custody, reads, work/time admission and JSON Pointer selection stay
  with the future enclosing retained-response walker.
  The inert Pointer Walker composes that scanner with 160 selection frames
  and a borrowed path. A turn checks at most 256 path bytes or polls the
  scanner once and handles its single event. Object segments compare lazily
  against decoded key scalars; array-token validation inspects at most 21
  bytes, so long segments are not rescanned up front for each branch.
  No key, value tree or selected-range list is allocated. A state-size test
  bounds the combined scanner/selection owner at 16 KiB; this is a measured
  component ceiling, not combined qualification with the emitter and file
  read/output buffers. Integration must account for those simultaneous owners.
  Work scales with retained bytes plus initial path validation; each key
  scalar costs constant matching work and array tokens have a fixed bound; wildcard expansion also consumes the
  enclosing retained-output quota. Neither an empty wildcard nor a long path
  may reset the original work/deadline allowance.
  The retained-file Resolution adds one fixed 4096-byte input buffer and
  borrows caller copy scratch, using at most 4096 bytes per copy turn. It
  keeps no selected-range list or response-sized tree. Each poll and finish
  charges the existing retention foreground-record meter and checks its
  original deadline, including CPU-only path validation. File reads and writes
  additionally charge I/O/output work; expanded values share the same response
  disk cap and a caller-supplied expansion ceiling preserving mandatory space.
  Each scanner event costs a CPU record; a source refill adds a read record;
  each copy turn spends CPU, read and append records, and each delimiter an
  append record. Selected bytes are reread even if still buffered. Large
  token/reference chains must fit the shared work budget before activation.
  Cancellation attempts truncation of only the unpublished tail; terminal
  failures leave it for whole-file cleanup. Neither path releases the owner's full-cap reservation. Combined runtime RSS qualification
  and dispatcher execution-phase admission remain pending.
  The first Core batch worker stores a bounded vector of resolved argument
  token/range pairs, at most one per top-level request argument and therefore
  bounded by the admitted token arena. Allocation uses fallible reservation.
  It streams argument values and member names through the same shared JSON
  engine as the owned emitter; immutable arena borrows pin source identity.
  Reads, copies and emission use at most 4096-byte scratch windows; small
  fragments are separate writes, not accumulated into full windows. Argument
  collisions use a linear merge of sorted names before resolving references,
  charging each member and at most 256 compared bytes per turn. Capability
  membership and the at-most-64 call-token vector are collected once.
  Reference field scans charge each member. Path conversion charges each
  256-byte chunk before one bounded UTF-8 validation scan. Successful echo
  arguments copy expansions into their result object, so expansion bytes can
  occupy the file twice, plus framing. Previously published expansions remain
  charged even when a later reference or method fails. Optional output leaves the entire
  mandatory preflight amount unavailable. The execution-phase check uses the
  retention clock without replacing its enclosing deadline or work budget.
  The final Response encoder borrows the completed index, arenas and owned
  file. Its state holds one borrowed arena emitter and one string frame, with
  no full-response buffer. Count and output share framing; count uses stored
  argument lengths without I/O, and output reads at most 4096 bytes per turn.
  Session escaping yields after at most 256 string-frame steps. Every turn
  charges the original meter and deadline before and after work; output does
  not acquire a replacement allowance. Counting proves wire length, not
  sufficient remaining work for transmission: argument reads and small output
  windows can require more turns, and even empty polls consume a record.
  Each phase yields separately, including short literal fragments; callers
  may coalesce successful fragments before transport. Exhaustion after headers
  requires aborting the exchange. Runtime peak RSS and largest accepted
  chain qualification remain pending.
  Each Push policy walker holds a 4096-byte input window, fixed scanner
  state and one 256-byte decoded direct-child key. Up to two walkers and
  five saved direct-child descriptors may coexist. Individual ID/property strings use
  at most 1538 raw bytes and one shared arena token; accepted IDs are ASCII
  and at most 255 bytes. Requested absent IDs retain at most the configured
  object count of names (256 bytes plus one usize each) for deduplication,
  with one fallible reservation before scanning the list. Each name comparison is
  charged; the bounded linear duplicate scan may use quadratic record work.
  The normal configured method object limit, not source JSON size, bounds
  this metadata. Scanning large nested create/update bodies retains no tree.
  A normalized argument object is another charged private file extent, so
  Push can retain expansions, normalized inputs and final policy output
  simultaneously. This adds no disk allowance or response reservation.

  Earlier method results and created-ID maps use the bounded disk retention
  contract in ADMISSION.md, never extra per-method heap trees. Event streams
  use slots without pinning storage views between emissions.
- Body job: headers, 512 bytes per MIME part and its projections, 512 KiB
  decode/metadata/output scratch.
  Nested parsing and transfer decoding share that reservation.
- Read view: 292 KiB caller-owned key/value/cursor scratch and one cold
  PostgreSQL snapshot. Database-client bounds are specified in POSTGRES.md;
  the process planning entry is not a database-server heap reservation.

Message size, upload/queue quotas, queue length and log file limits are disk or
admission bounds. Growing them does not reserve whole bodies or a whole queue in
RAM. The default log disk reservation is five 8 MiB files (active plus four
retained). ADMISSION.md freezes logical byte/file quotas, logical completion
reserves and maintenance work/deadlines, enforced by M05/M08 before mail can
be accepted.

The per-upload byte ceiling is `message_bytes`, at most 32 MiB; M13 publishes
that value as `maxSizeUpload` and enforces it even for attachment uploads.
`upload_disk_bytes` is the separate aggregate quota for bodies retained by
upload leases.

The compiled maxima and checked default values live in `limits.rs`; this table
records the default byte ledger only. Transaction/WAL limits are fixed to the
storage contract. SMTP retains room for at least 100 recipients. Disabled event
streams may use zero slots; mandatory pools and byte budgets cannot be zero.
The bounds are startup validation, not protocol error mappings or proof that
all combinations meet standards. ADMISSION.md fixes operation work limits;
POLICY.md adds parser/search field limits. M13 publishes only limits that its
admission code actually enforces.

## Fixed execution ownership

This is a proposed runtime layout, not running workers. Its exact counts,
quanta and buffer partitions may change during integration under DESIGN
section 5. Bounded concurrency, ownership, completion delivery and foreground
progress remain requirements. The main thread
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
| Body/read | 2 | SMTP/HTTP parsing, JSON tokenization, auth verifier work, JMAP dispatch/serialization, streaming blob I/O, MIME, reads/search and per-object planning; yields by bounded chunks |
| TLS handshake/dial | 2 | Bounded connect_timeout and handshake steps; moves a transport slot back to main after success |
| Logger | 1 | Fixed event queue drain, bounded encoding and file rotation |
| Resolver | 1 | A/AAAA DNS packet I/O, TTL cache and CNAME parsing with fixed deadlines |
| Control | 1 | Configuration/device/ACME/administrative jobs and backup coordination |

A configured larger body-job or handshake pool increases resident job
slots, not thread count. Workers resume bounded steps across their
assigned slots. A job retaining an incremental body handle or a MIME owner
borrowing it stays on the worker that opened it until close. Pending
releases execution time, not that job's thread affinity; main exchanges
only scalar completion data and caller buffers, never the body owner. A
waiting SMTP/HTTPS peer never occupies a worker while idle after dial: retain
state in its existing slot and return Pending to main. The SMTP/HTTPS dial
exception is std's blocking connect_timeout: only the sole outbound slot
may dial, so at most one of the two handshake workers is occupied by it.
The other remains available for inbound handshakes. DNS packet exchanges
separately occupy the sole resolver worker during bounded UDP/TCP waits;
they do not borrow handshake workers or create replacement threads. Every
DNS wait retains the original job deadline and the per-operation idle cap. A pending handshake is
dispatched again after ten milliseconds, at most one queued/running step
per slot. Each step does bounded nonblocking I/O/crypto and returns one
completion; it never waits for socket readiness. The compiled
eight-handshake maximum thus admits at most 800 such steps/second, within
the fixed queues and global completion-credit rule below. This polling
cost is an unmeasured M07/M23 acceptance obligation. Disk I/O may block
its fixed worker; it cannot create replacement threads. Deadlines prevent
accepting more work behind a failed/stalled resource, but cannot promise
cancellation of a kernel I/O hang. An unexpected worker failure stops new
mutations and fails health; no panic catching is used as transaction
recovery.

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
Writers never wait for another worker while holding the commit lock. Body
sources must be ready for bounded synchronous reads when the writer begins
the atomic body/metadata transaction; reservations remain charged while their
slot waits. Checkpointing uses the writer's own buffers.

Body/read workers advance protocol CPU work by at most 16 KiB input/output or
256 parser/token/object transitions per step, whichever comes first. Parsing a
whole request is not one main-thread operation. Main only does bounded socket
record work, scheduling, and small HTTP-01/event/health framing from existing
validated state (at most 2 KiB of that control framing per turn). Worker jobs
resume in rotating order among each worker's assigned slots; an idle job
retains affinity but does not occupy execution time.
Use bounded queues with explicit saturation. Sorting must have a complete
deterministic tie-break and bounded input/scratch; allocation is permitted.
Typed adapter errors carry
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

## Direct receiving runtime

The initial `smtp_receiving` runtime implements direct IPv4 SMTP with two
fixed workers: storage and TLS. It does not instantiate the other roles in
the proposed whole-service table above. Standard worker stacks and bounded
per-connection allocations are permitted under DESIGN section 5; this
increment makes no new whole-service memory claim.

A startup-reserved vector holds at most the configured SMTP connection
count (compiled maximum 32). Each mutex-protected slot owns its protocol,
transport, optional ingress job and one phase: empty, network, queued,
completed, retry or retiring. Worker scans replace separate work queues:
each worker performs at most one operation per eligible slot per pass.
Only the matching worker executes a queued operation. Main acknowledges
completed phases before further socket work or redispatch. Reuse requires
worker retirement followed by main releasing the peer/listener count.
There are no queued references that can outlive this phase, so these slots
need neither scalar reuse tokens nor a separate completion-credit ledger.
The retained slot is the completion reservation before every effect.

Main uses nonblocking slot locks and a round-robin socket scan. Threads
use park/unpark with a maximum five-millisecond wait. Pending TLS turns
are rescheduled no sooner than ten milliseconds after completion and keep
the original handshake deadline. Storage coordination retries retain the
captured operation deadline and wait at least five milliseconds after a
contention result. Plain/TLS socket progress and SMTP parsing
use the existing bounded Network turns. Storage consumes one decoded
line per turn; header completion and atomic native publication retain
their existing configured byte/work/time bounds, not a new parser quantum.

The storage worker creates and retains its worker-local entropy handle.
TLS session construction precedes 220; the single shared handshake pool
and connection slots bound native owners and fixed wire buffers. The storage
worker retires normal connections; the TLS worker disposes failed upgrades
it owns. Movable TLS state has no thread identity requirement, and ordinary
spool cleanup cannot hold the TLS worker behind a storage commit. Main
never drops a live delivery or native TLS owner during its network loop.
After worker failure, the loop stops and joins both workers before
terminal cleanup, including disposal of poisoned
slots without resuming their operations. Blocking kernel I/O remains a
host failure that no timeout can forcibly cancel.

## Scratch partitions

Partitions below are proposed capacity estimates. Implementers may reuse
storage only when its lifetimes permit it. Existing fixed-buffer APIs enforce
their supplied capacities; larger or allocating representations may be chosen
explicitly under DESIGN section 5 without a speculative ledger proof.

| Reservation | Partition |
| --- | --- |
| Body work, 512 KiB/job | Bounded part metadata, one 192 KiB generated fragment, decode rings, normalization/parser state and output staging; header and per-part arrays are separate |
| Read cursor/value, 292 KiB/view | 68 KiB row input/reserve; 64 KiB retained result value; 1 KiB key; 5 KiB selected metadata; 1 KiB pool bookkeeping; 4 KiB pinned-body input/reader state; 149 KiB typed row/change cursors, native wrapper bookkeeping and reference validation scratch |
| Outbound scratch, 128 KiB | 64 KiB body transfer; 16 KiB reply assembly; 16 KiB SMTP/TLS handoff state; 32 KiB transaction-planning/ID/diagnostic scratch |
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

Fragmented delimiter scanning shares the resident core and keeps its exact
extent length in the fragmented owner. The resident core derives length
from its retained source slice, preserving the traversal layout below.
Both public owners retain the existing 160-byte ceiling. No source buffer
is retained by the fragmented owner. The shared suite tests fragments
including bytes past the extent, exact I/O and record cuts, CR handling
and original-meter handoff. Worker/source buffers need separate
reservations; this is not native stack or RSS qualification.

The composable delimiter Scanner fits 192 bytes and copies at most 70
selected-boundary bytes under one funded record and one visit per byte.
Its owner label binds later turns to the original Meter across moves.
The native blob_delimiters owner fits 512 bytes, including that scanner;
caller input backing and the retained native snapshot are separate. Each
native turn reads at most 4096 bytes or scans at most 128 body bytes, never
both. Reads charge their requested bytes and one record; scanner turns
charge their existing costs. No heap allocation occurs in the adapters by
inspection; this is not a native-allocation, stack or RSS measurement.
The existing resident traversal's layout and reservations are unchanged.

Clipped entity header collection reuses root_headers' existing 512-byte
owner ceiling and bounded read/scanner turns. The source base adds no new
backing. finish_split returns the consumed header witness plus at most one
already-read lookahead byte and releases the disjoint remainder for another
entity. There is no allocation or copying in this split. Callers reserve
aggregate resident witness capacity independently of recognized-header
accounting; tentative names and liberal non-header terminators can retain
bytes outside that recognized count. This does not create or qualify a
whole-worker arena, native stack or RSS reservation.

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
retained snapshot copies also fund their actual cell bytes. Identity sizes
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
fits within ../td-mime/UNICODE.md's future 256-byte decoding cursor checkpoint. No
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
one test-only compilation of production sources. Cold database setup,
pin verification, resident-source construction, original mapping and all
backing are outside counting; binding ownership and PinnedBlob snapshot-pin
teardown are inside. Enclosing PooledRead/CommittedView and scratch lease release
remain outside. Healthy zero/near-u64 bases, partial/completed/Bound expiry,
same-length source mismatch, original I/O refusal and constructor expiry
leave every Rust allocation counter unchanged. Only these eight intervals
are qualified; the remaining source-binding cases listed in API §1.127
are unmeasured. Existing 65 metadata/locator trials remain separate. Native allocation, worker stacks, RSS,
complete request admission and access-policy/publication wiring remain
unqualified.

M06dw original source-bound part-member emission consumes only Bound and
retains its actual snapshot pin plus all original source/windows/candidate
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
original source/window/candidate and actual snapshot pin/pool borrow.
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
actual snapshot pin, prior source/windows/candidates and separately
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
member cells and actual snapshot pin. Every turn funds one original
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
collection, source/fragments and member cells with the actual snapshot pin
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

M06ee whole requested tree/list retention keeps the compiled 1 KiB
Cursor plus HeaderBudget and 768-byte Retained caps. Whole output is
separately caller-owned/admitted. API §1.136 names eight allocation
intervals, with cold original preparation/enclosing resources excluded
and descriptor teardown counted. Earlier 137 intervals remain separate;
native allocation, stack/RSS/full admission remain unqualified.

M06ef source-bound part-field selection keeps the original
Bound/ordinal, fixed nine-span index and pure Properties. The shared
base Cursor also carries optional inline index state on legacy ALL
paths. Cursor plus HeaderBudget, 64 output bytes and five segment
references fits 1 KiB; Member fits 768 bytes. Whole backing/output
remains separately caller-owned/admitted. Index at most 64 generated
bytes per turn with exact interpretation and record/control funding;
copy at most 64 bytes with exact wire debit and no source/I/O work. API
§1.137 qualifies eight enumerated Rust allocation intervals with cold
original preparation and counted descriptor teardown; earlier 145
intervals remain separate. Native/stack/RSS/full admission and complete
request/publication paths remain unqualified.

M06eg whole selected source-bound part fields reuse the shared Window
beside original custody, fixed nine-span index and explicit Properties.
Cursor plus HeaderBudget, five segment references and 64 output bytes
fits 1 KiB; Retained fits 768 bytes. Caller output/backing remains
separately owned/admitted. Whole retention adds no interpretation,
record, wire or source/I/O work beyond selected emission. API §1.138
qualifies eight enumerated Rust allocation intervals with cold original
preparation and counted descriptor teardown; earlier 153 intervals stay
separate. Native allocation, worker stack/RSS, full admission and
complete request/publication paths remain unqualified.

M06eh selected-field collection keeps the shared Cell within 64 bytes,
Collecting plus active Child and HeaderBudget within 2 KiB, and
completed Serialized within 768 bytes. Original backing, windows and
cells remain separately caller-owned/admitted. Each child preserves
selected emission's exact five-cost debits; accepting one complete
original ordinal adds the unchanged one step and two records (one record
plus zero-credit control), with zero source/I/O/wire fees. NONE does not
waive acceptance work. API §1.139 qualifies eight enumerated Rust
allocation intervals with cold original preparation and counted
descriptor teardown; earlier 161 remain separate. Native allocation,
stack/RSS, full admission and complete request/publication paths remain
unqualified.

M06ei selected-field composition keeps Cursor plus HeaderBudget and 64
output bytes within 1 KiB and completed Composed within 768 bytes. Reuse
the shared bounded frame, funding, at-most-64-byte copy and original
admitted backing/cells/windows. Empty metadata only changes the static
provisional structural separator. API §1.140 qualifies eight enumerated
Rust intervals with cold preparation and counted descriptor teardown;
earlier 169 remain separate. Native allocation, stack/RSS, configurable
subParts selection and full admission/publication remain unqualified.

M06ej requested selected-field composition keeps Cursor plus
HeaderBudget and 64 output bytes within 1 KiB and completed Composed
within 768 bytes. The two pure selections add no buffering or parallel
frame; original admitted backing/windows/cells remain separate. Outer
NONE spends no emission work; other requests preserve shared exact
five-cost charges. API §1.141 qualifies eight enumerated Rust intervals
with cold preparation and counted descriptor teardown; earlier 177
remain separate. Native allocation, stack/RSS, configurable subParts
selection and complete admission/publication remain unqualified.

M06ek whole requested selected-field retention keeps Cursor plus
HeaderBudget within 1 KiB and Retained within 768 bytes. Reuse the
original bounded retention Window, composer and all five exact cost
debits; output, original backing, generated metadata windows and
collection cells remain separately caller-owned/admitted. API §1.142
qualifies eight enumerated Rust intervals with cold preparation and
counted descriptor teardown; earlier 185 remain separate. Native
allocation, stack/RSS, configurable subParts selection and full
admission/publication remain unqualified.

M06el decoded body-part property recognition keeps Cursor within 192
bytes and Property within 64 bytes. The existing header selector owns
parameterized syntax; standard fields prepay one static-table row per
poll with at most eleven byte visits and one record, followed after
twelve misses by one terminal record with zero byte visits. Long non-header
keys use constant table work with no copied key. API §1.143 qualifies
eight enumerated Rust intervals with long key/refusal-state setup cold;
earlier 193 remain separate. Native allocation, stack/RSS, request
aggregation/admission and current authenticated publication remain
unqualified.

M06em decoded body-properties aggregation keeps Cursor within 384
bytes and Cell within 64 bytes. Original decoded keys, their array and
fixed header cells remain caller-owned/admitted. Recognition retains
§1.143 bounds; deduplication prepays at most 128 visits and one record
per 64-byte two-sided comparison. No second key or dynamic header list
is allocated. For N requests, H distinct header keys and maximum length
L, this charged quadratic scan uses O(N H ceil(L/64)) comparison turns
and O(N H L) visits; equal-length distinct keys walk H(H-1)/2 pairs.
Fixed cell capacity and meter quotas bound the total. Cells retain their
key-loan lifetime, including untouched stale tails; reconstruct them
after old loans end before changing request arenas. API §1.144 qualifies eight enumerated Rust intervals with
long-key/refusal preparation cold; earlier 201 remain separate. Native
allocation, stack/RSS, whole request admission and current authenticated
publication remain unqualified.

M06en JSON argument decoding keeps Cursor within 256 bytes and Cell
within 16 bytes. Caller source, String capacities, cells and key slots
are separately admitted cold storage. Each poll prepays at most six
input visits, one record and four decoded output bytes; terminal EOF
spends one record, zero visits and performs no text scan. Scalar output
checks actual capacity before push; no hot String growth occurs.
Consumed cells must be reconstructed after decoded loans end. API
§1.145 qualifies eight isolated Rust allocation intervals; earlier 209
stay separate. Native allocation, stack/RSS, full request admission and
current authenticated publication remain unqualified.

M06eo's joined argument/semantic selection Cursor fits 512 bytes. Text
capacities, key-reference slots and header cells remain separately
caller-owned/admitted. Each turn forwards one original parser poll;
paid JSON EOF transfers decoded loans without key scans/copies or a
second charged turn. Original maxima are 184 input visits, 32 records
and four output bytes. Default completion does no work. API §1.146
qualifies eight Rust intervals; earlier 217 remain separate. Native
allocation, stack/RSS, whole request admission and current authenticated
publication remain unqualified.

M06ep's selected subParts Cursor plus HeaderBudget and 64-byte output
fits 1 KiB; Composed fits 768 bytes. Original fragments/cells and file
remain separately admitted. Shared framing keeps one aggregate step per
turn, one job record every sixteen turns and at most 64 prepaid wire
bytes, with no source/input visits. Omitted subParts does not descend
into root children; original classification still supplies requested
lists. API §1.147 scopes eight Rust intervals with collection and
constructor-refusal setup cold and in-interval expiry transitions counted;
earlier 225 remain separate. Native allocation, stack/RSS, header rendering,
whole request admission and authenticated publication remain later.

M06eq keeps selected subParts retention Cursor plus HeaderBudget within
1 KiB and Retained within 768 bytes; caller output remains separately
admitted cold storage. Reuse the same Window, bounded framing and five
cost domains; exact capacity completes and short windows never expose
whole output. API §1.148 qualifies eight Rust intervals with original
storage/output/refusal preparation cold. Clock expiry transitions remain
inside measured intervals; earlier 233 remain separate. Native
allocation, stack/RSS, full request admission and authenticated
publication remain unqualified.

M06er keeps the source-ordered header-array Cursor plus HeaderBudget within
512 bytes and original-budget Completion within 64 bytes; transient caller
output stays separate. Scanner turns retain the 255-visit/256-step bound;
Raw/string projection stays bounded and literal copies are at most 64 prepaid
wire bytes. No source/field/value/output storage grows. API §1.149 scopes
eight Rust intervals with source/budget/output preparation cold and
construction/polling/consuming refusals measured. Earlier 241 intervals remain
separate on the accepted retention parent. M06es below qualifies whole-array
fixed-window retention. Native allocation, stack/RSS, selected body-part
header
wiring and full original pin/request/publication admission remain unqualified.

M06es bounds the fixed-window whole-header Cursor plus HeaderBudget to
640 bytes. Caller output is distinct fixed cold storage; retained values borrow
its original prefix and leave spare capacity untouched. Scanner/Raw/JSON
bounds and all charges match streamed emission. Fresh admission precedes
capacity; a full healthy Window may mask the next source refusal without
refunding original charges. No field table, decoded
string or growing output is added. API §1.150 scopes eight Rust allocation
intervals with source/budget/output preparation cold and construction,
polling and consuming refusals measured. Earlier 249 intervals remain
separate on the accepted streamed-header parent. Native allocation, stack/RSS,
header selection/forms, full original pin/request/publication admission
remain unqualified.

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
Service implementations must enforce admitted input, work, disk and
concurrency limits. Speculative memory partitions do not block implementation.

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
measured. Rust/native allocation, complete generation/session coexistence
and whole-process RSS remain unmeasured service dimensions. Under DESIGN
section 5 they do not create a byte-ledger prerequisite for functional work.
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
there is no physical-filesystem binding or free-space probe. PostgreSQL owns
durable body/metadata transactions. The active local ledger charges response
files only; database quotas are transaction-bound as POSTGRES.md specifies.

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

PostgreSQL body streaming, server budgets and native qualification are
specified in POSTGRES.md. Removed embedded-database observations are not
PostgreSQL evidence. The bounded client I/O chunk is 64 KiB.

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
or rebuilt. Ticket issuance makes at most 64 atomic compare/exchange
attempts, each against the value the previous attempt observed; a lost
attempt means another issuer took a ticket. Losing every attempt returns a
temporary Contended error without reserving a slot, and exhaustion
permanently refuses issuance before wraparound. Independent pools share the
counter, including parallel tests' coordinators. Generations do not
authorize account data and cannot be persisted or accepted from peers.

These primitives add no worker, locks or payload ownership enforcement.
A scheduler using these token primitives must use this lock protocol:
briefly lock the pool and resolve the token to locate its payload lock, then
release the pool lock before acquiring
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
M23 measures integrated service memory; this diagnostic supplies no service
memory claim or exhaustive pre-integration qualification requirement.

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

The fixture explicitly declares fifteen SMTP slots, a 512 KiB disposable
index cache and a 128 MiB planner budget so the fifteen one-slot gateway
listeners are admitted. It supplies an
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
delta is about 1.2 MiB in both counter domains, compared with the generation
estimate. Its single HTTPS listener has no views to share across listeners.
Integrated memory measurements remain M23 work; this fixture alone supplies
no service memory claim.
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
threshold or refusal-memory peak. Session/counter allowances remain planning
estimates; configured queue limits are not a whole-memory bound.

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

## HTTPS authentication runtime

The mailbox-access profile adds fixed HTTPS network, TLS, authentication and
body workers to the receiver/control threads. A startup vector contains at
most https_connections mutex-protected slots (compiled maximum 32); each
occupied slot holds two 18,437-byte TLS wire arrays, a 32 KiB request head,
a 16 KiB plaintext input buffer, bounded response headers (under 512 bytes),
an optional fixed 2 KiB event frame plus stream bookkeeping, deadlines and
transport state. The client also owns the bounded decoded credentials while
authentication or body work needs them. Ordinary responses and EventSource
clear both the credential head and decoded owner before retaining output;
other client retirement also clears them.
Consumed input is cleared after each framing turn; bytes beyond the head
remain in the same buffer until body dispatch or refusal. All input is
cleared on retirement. These are ordinary buffer hygiene measures, not
guaranteed erasure of provider/compiler copies. Native TLS, thread stacks
and allocation overhead still require integrated M23 measurement; no
whole-service memory qualification is added here.

The TLS worker constructs sessions and visits each eligible handshake at most
once per ten milliseconds, under the receiver's shared HandshakePool. Network
and TLS scans wait at most five milliseconds between passes and never wait on
socket readiness. Each nonblocking network turn reads at most 16 KiB, parses
one head turn or advances one bounded response. The network thread admits
origin/rate checks and hands credential lookup to the fixed authentication
worker. The current local backend tries its registry mutex there and performs
no credential file I/O. A network-owned 32-entry peer table avoids locking TLS slots for admission;
a retired slot can retain its old peer charge until the next scan. The
listener has a fixed two-connection per-IP cap.
Transient accept resource failures back off for five seconds. Session slots
are the completion storage; no unbounded work queue or per-client thread exists.

Control publishes credentials on its existing fixed worker while holding the
registry mutex. The snapshot has a 64 KiB file cap and at most 256 records;
control request/reply buffers have 256-byte/12-KiB caps. Successful publication
precedes password output. Authentication attempts use a fixed 64-entry table,
64 attempts globally and eight per IP per one-second monotonic window. Native
work already executing is joined on shutdown, not forcibly canceled. The
current HTTPS profile uses a separate fixed body worker for uploads, raw
downloads and retained Core API responses; ADMISSION.md owns those lifetimes.
Discovery retires the 32 KiB credential head and decoded credentials before
allocating its at-most 32 KiB Session response. EventSource retains only its fixed frame and passive
snapshot; it never holds a store view or a response file. The prepared
Session is shared startup metadata. These bounds fit the existing per-client
scratch reservation; integrated whole-service measurement remains pending.

The shared handshake pool bounds work, not availability against deliberate
starvation: one peer sending partial TLS records on two connections can hold
both default permits until their deadlines, including permits SMTP needs.
The no-byte peek avoids charging silent TCP peers; it does not validate a
ClientHello or provide cross-protocol fairness. After flushing a refusal,
HTTPS closes the write half and drains at most 64 KiB of plaintext, including
the retained head tail (or the whole final read after a framing failure),
within one fixed five-second close/drain deadline
clipped to the original session deadline. Each turn reads at most 16 KiB;
received data never extends the deadline or reenters HTTP parsing. A valid
Content-Length further caps discard to its unread body bytes, so a complete
or absent body does not wait for peer EOF. Chunked or malformed requests
use the byte/time/EOF bounds. Draining slots continue to count against both
global and per-peer connection limits. Pending reads do not clear an already
empty input buffer. This
reduces resets caused by unread bodies without waiting for a body before
sending the final response (including Expect: 100-continue requests). A peer
that exceeds either bound can still observe a reset. Service shutdown
retires these refusal-only slots immediately; no body is committed.

## Private response retention boundary

RequestSpool owns 32 fixed filesystem slots with atomic occupied/retired
bitsets, borrowing a separately locked root. It allocates no response heap.
ResponseCoordinator reserves the full per-request response cap in its existing
ledger, preserving one lease cell for physical publication or maintenance.
Default aggregate capacity admits two such reservations; CONFIG.md records
the deliberate concurrency tradeoff and future advertisement obligation.
Retired slots remain occupied, and uncertain cleanup retains the full cap
as used until quiescent cold recovery. Slot status is not another byte quota.
Append and exact published-prefix reads take caller buffers of at most
64 KiB and charge the configured foreground I/O/record budgets, including
replay; truncating a provisional tail charges one record. Retention uses
the original outer exchange deadline, with tighter phase budgets still the
future dispatcher's responsibility. Cleanup can outlive that deadline.
These inert boundaries do not qualify HTTP dispatch, response indexing,
reference walking or whole-service memory/scheduling integration.

## Native entity-header owner

The composed native entity-header cursor has a compiled 4 KiB owner ceiling.
It moves the original pin and work borrows between collection and shared
interpretation without allocating, reserving caller header backing and the
native connection separately. The existing collector retains its 4096-byte
native-read bound; interpreter turns retain shared header/parameter bounds.
Each poll advances one bounded collection or interpretation turn, including
its consuming handoff on completion. Boundary output is at most 70 bytes and
uses the original work budget. A handoff returns original live owners and
unused backing; it reserves no new job or interpretation quota. This is not
whole-tree or whole-service memory qualification.

## Native complete MIME owner

The native structure cursor plus HeaderBudget has a compiled 16 KiB ceiling.
The frame array and entity interpreter remain in the shared cursor; no second
traversal or whole-body buffer is added. Callers reserve descriptor slots,
matching optional header-view slots, retained header backing and one nonempty
body buffer separately. Reads use at most min(4096, body-buffer length) bytes.
Header collection retains its existing read/scanner bounds and consuming
witness handoff. Recognized header limits do not replace actual witness-backing
capacity; an insufficient arena or slot set refuses the whole operation.

Each poll performs one bounded read/parser/validation turn. Correspondence
validation checks one descriptor and header view for one work record; cached
completion is freshly fenced without positive work debit. Output accounting
includes each retained header-view slot beside shared part descriptors and
boundary/decoder output. Rewinds and overlapping parent/child scans charge
requested repeated read bytes (including any short read) and parser visits.
This component does not qualify whole-service
memory, native allocator maxima or Mail capability advertisement.

Descriptor and header-view capacities of mime_parts cover every successful
tree. At each header request, the remaining arena must first hold the smaller
of the clipped entity length and remaining recognized-header limit plus two;
subsequent tentative header bytes may need more. The immutable consumed prefix
and its actual lookahead witness stay reserved across later requests. No bound
from recognized header bytes alone guarantees successful witness retention.
A checked product of maximum part count and total source extent length is a
conservative backing bound, but can be impractical; the enclosing job instead
admits a fixed arena ceiling and explicitly refuses when it is insufficient.

## Native retained part metadata

The native part cursor and HeaderBudget fit a compiled 8 KiB ceiling, including
the shared part-header/label/location cursor. All output windows, original
header prefixes and normalization scratch remain caller-reserved. Parser turns
retain their shared bounds; final classification is a fixed set of media and
disposition comparisons and charges the same Class/Node bytes as resident
classification. No body buffer, read, allocation or new quota is introduced.

The current consuming metadata handoff ties output-window borrows to its live
work/header/scratch lifetime. A chain of N parts therefore retains all N sets
of six output windows; it does not permit a single window set to be recycled
between these handoffs. N is bounded by mime_parts. The enclosing job must
check and admit the sum of all chosen window capacities, plus cumulative
header backing and the fixed cursor/scratch reservations. The 8 KiB owner
ceiling is not a bound on that aggregate message storage.

## Native part member JSON

The native member encoder plus HeaderBudget fits a compiled 2 KiB ceiling.
Its shared scalar/frame implementation emits at most 64 bytes per turn into
caller-owned output and retains the existing interpretation and output charges.
The resident wrapper stays within its existing 1 KiB ceiling and owns only
its original parent failure/progress references around that same encoder.
No buffer, scratch window, source read, allocation or resource limit is added.

### Native direct-leaf locator candidate storage

The native locator cursor retains one original traversal handoff and one
borrowed candidate span. Its compiled ceiling, including HeaderBudget and one
Candidate, is 1 KiB. The enclosing job must reserve exactly N candidate cells,
where N is the bounded native descriptor count, and must admit the checked sum
alongside the retained headers and descriptors. Candidate remains at most
128 bytes. No message-sized copy, heap allocation or new allowance occurs.
Each turn funds one shared mapping: one explicit record, one interpretation
step for multipart or the fixed wire length in interpretation steps and output
bytes for a leaf. Interpretation steps also fund records through the shared
Credit policy; those records are additional to the explicit mapping record.
Each mapper starts empty Credit and retains it across its parts; unused
prepaid steps are discarded at finish, with no refund or new allowance.
Native clock fences
and zero charges are fresh even after mapping is complete. Original source
bytes and I/O budgets are unchanged; no body read occurs.

## Native body-list selection storage

The native list cursor and HeaderBudget fit a compiled 3 KiB ceiling; the
cursor also fits the shared selector plus 256 bytes. Fixed inline state holds
both optional phase slots, with at most one occupied, without heap allocation. The
caller must reserve and admit nodes, text/HTML/attachment ordinal spans and
membership bytes alongside retained headers and descriptors. Output windows
share the admission-owner lifetime and stay borrowed through the handoff.

Correspondence checking costs one original work record per node. The shared
selector retains its existing interpretation, record and output charges and
bounded turns. Deadline checks and cached completion add no positive charge.
No source byte or native I/O debit occurs because selection reads only the
retained classifications. This adapter does not implement whole-job memory
admission or authorize Mail capability advertisement.

After N correspondence turns, one constant-size handoff turn constructs the
shared selector and yields without a positive charge. Selection starts on the
following turn; this single transition does not scale with message size.

## Body-value option decoder storage

The body-value option cursor has a compiled 2 KiB ceiling, borrows four raw
argument spans and allocates no input-sized storage. Its fixed JSON scanner
accepts only scalar option types. Aggregate input is at most 4 MiB; the
caller admits that retained input alongside the request's other backing.
Exact decimal conversion uses the existing JMAP integer parser, shared with
changes/query argument handling; its coefficient counters rely on this cap.

Each omitted argument or scanner initialization costs one original record.
Each subsequent poll costs one record plus the requested input span length
(at most 64) as I/O bytes, charged before scanning. Scanner event boundaries
can leave part of a requested span unconsumed, so revisiting those bytes is
conservatively charged again. Numeric conversion sees only consumed bytes.
Cached completion and explicit deadline checks add no positive charge.
Output/unlink allowances are unchanged. No HeaderBudget, quota reset, source
read or whole-job memory-admission claim is introduced.

## Native selected metadata storage

The native selected-metadata cursor, HeaderBudget and one 64-byte output
window fit a compiled 1 KiB ceiling. It borrows the original generated fragment
and candidate and retains the original traversal owners and scratch. The
caller must admit retained fragments and candidates alongside the header arena,
descriptors and response storage; the cursor does not perform whole-job backing
admission. No heap allocation, body read or new quota occurs.

The shared selector preserves resident charges: one record per index/output
turn, one interpretation step per output turn or per indexed byte, and exact
written output bytes. Interpretation steps additionally fund records through
the retained stage Credit. Empty selection, empty output and cached completion
add no positive charge. Fixed constructor checks compare retained scalar
identity/range facts and freshly check the original deadlines. Partial
selection indexes the original generated fragment in at most 64-byte turns;
all-field selection copies the already-generated member sequence directly.

## Native body-property composition storage

The native body-property cursor, HeaderBudget, one fragment descriptor and a
64-byte output window fit a compiled 1 KiB ceiling; each fragment descriptor
fits 128 bytes. The caller must admit the complete fragment-cell array and
retained generated bytes alongside original headers, descriptors, lists and
response storage. This adapter allocates no backing or whole-body copy.

Correspondence costs one original work record per part, including for an empty
outer selection. Empty output does not advance or add a positive charge.
After correspondence, the shared framer preserves resident costs: one
interpretation step per turn, Credit-funded records, and exact written output
bytes. Every copy is at most 64 bytes; fixed traversal state bounds the
multipart walk. Cached completion adds no positive charge. Original source
and native I/O allowances remain unchanged; no body read, quota reset or
whole-job memory-admission claim is introduced.

## Native body-value selection storage

The native selector plus HeaderBudget fit a compiled 1 KiB ceiling. Callers
admit one metadata-fact cell and one membership byte per original part, plus
an ordinal output window. No allocation or body read occurs. Each turn visits
one part/list entry or performs one fixed phase transition. Work is linear in
parts plus enabled-list entries; output order is original preorder and union
membership removes duplicates without an additional scan per part.

Each active turn charges one interpretation step and one explicit original
work record; interpretation Credit remains in the cursor. Each emitted u16
additionally charges two output bytes before writing. Cached completion and
deadline checks add no positive charges. Initialization overwrites every used
membership cell before any value can complete. Short ordinal capacity refuses
without growing or replacing backing; partial cells remain provisional.

Interpretation additionally charges one original record per 16 prepaid steps,
rounding up. For T active turns, the total record debit is T + ceil(T/16).
Unused stage credit is discarded at finish rather than transferred or refunded.

## Native body-value encoder custody

The native cursor fits a compiled 2 KiB ceiling and owns no heap allocation.
Its typed child preserves the existing encoder state and at-most-47-byte
output chunk. The caller separately admits transfer input (nonempty, at most
6 KiB), fixed checkpoint storage, original header/metadata backing, scratch
and response windows. No whole-body buffer or reopened native pin is created.

Transfer and charset scans, plain filtering, HTML planning/replay and JSON
framing retain the existing work charges and original Meter. Native deadline
checks add no positive charge, and this stage does not reset or spend header
interpretation allowance. Typed consuming release is passive and available
only after complete healthy encoding; the native owner freshly checks before
releasing those original borrows. Remaining input bytes/checkpoints are
returned as their original windows, without copying or allocating replacements.

## Native bodyValues map storage

The parent fits 1 KiB and its single borrowed child fits 8 KiB, including
inline metadata and value cursor slots. Neither allocates. Reusable caller
storage comprises one set of metadata windows, one Scratch, one transfer
input window capped at 6 KiB and fixed checkpoints. These are additional to
the original admitted structure, selection/facts and response output window;
no per-selected-part value buffer or whole-body retention is introduced.

Each parent copy writes at most 64 bytes and charges one original work record
and interpretation step, plus its exact emitted bytes before copying. Stage
interpretation Credit additionally costs ceil(T/16) records for T framing
turns; unused credit is discarded at finish. Deadline checks, empty output,
NeedValue and cached completion add no positive parent charge. Child metadata
and decoding preserve their existing original-budget charges. Child bytes go
directly to the response window, without a second staging-copy charge.

Selection traversal and map framing are linear in selected parts and emitted
framing bytes. Metadata replay and body decoding retain their original bounds
and native I/O charges. This compositional ceiling is not whole-worker or
pinned-musl memory qualification.

## Email preview scalar storage

The plaintext preview cursor fits 128 bytes and retains no source or output
buffer. The external caller admits source decoding/HTML extraction and
response backing. Each active nonempty or EOF turn spends one original work
record; a generated separator turn also spends one record. Each emitted
scalar charges its UTF-8 length before returning it. The display retains at
most 256 scalars and charges at most 1024 output bytes, independently of the
source suffix length. A suppressed suffix still costs one record per input
scalar. Empty nonfinal polls, cached completion and explicit freshness checks
spend no positive allowance. The original Meter is never copied or reset;
the cursor's checked-clock watermark starts at the supplied prior job Tick.
Work remains linear in the entire chosen decoded source, even after the cap.
A large source may exhaust the enclosing job and refuse preview rather than
publish its prefix. Source selection and worker admission must account for
that availability tradeoff; this cursor claims no constant-time whole preview.

## Charged HTML extraction storage

The html_text cursor fits 384 bytes, including the shared fixed-state parser
and checked-clock observer, and retains no source/output buffer or heap state.
Each uncached poll spends one original work record, including input-gap polls,
reference resolution, suppressed source text and EOF draining. Each emitted
scalar additionally spends its UTF-8 byte length before exposure. Cached
completion and explicit freshness operations add no positive charge. The
original Meter is never copied, replaced or reset. The shared parser consumes
at most one source scalar and emits at most one scalar per turn, with bounded
name storage and reference table work. Total work remains linear in the chosen
source plus fixed overhead per reference; a refused budget never publishes a
successful partial result. This is component accounting, not whole-worker or
pinned-musl memory qualification.

The Meter's output_bytes covers intermediate transformations as well as wire
serialization. Extraction therefore charges the entire projected source,
including text later omitted by the preview cap; subsequent preview and JSON
stages charge their own output. It is not solely the advertised response-size
ceiling. The composing job must admit all stages; a large source may exhaust
this work allowance and refuse a small preview. No stage silently replaces or
increases that original allowance.

## Preview JSON composition storage

The preview JSON cursor fits 2 KiB and uses the existing caller-owned text
input storage (at most 6 KiB plus 2 KiB for checkpoints). It retains only fixed
reader, HTML, preview and JSON framing state, with one pending decoded and
one pending extracted scalar. It allocates no source-sized or result-sized
buffer. Each output call writes at most six bytes and leaves the remaining
caller slice untouched.

All stages borrow the same original Meter. Source decoding, extraction and
preview projection retain their existing record and intermediate output
charges, and JSON framing additionally charges its exact wire bytes. These
transformation charges intentionally accumulate: a plain one-byte `a`
requires five output-byte units, including the three-byte JSON string.
They are not interchangeable with the enclosing response wire-byte ceiling.
Full-source validation continues after the preview cap and can refuse on
budget exhaustion. Native snapshot and aggregate worker/response admission
remain obligations of the enclosing operation.

The output-byte grant must cover the text reader's decoding/validation
charges, plus all emitted extraction UTF-8 bytes for HTML, plus at most
1024 projected UTF-8 bytes, plus at most 1538 JSON wire bytes (two quotes
and a conservative six bytes per projected scalar). Reader source visits
and records retain their separate bounds, including charset prescanning.
The 256-scalar result cap does not bound the first two terms.

## Native preview custody storage

The native preview cursor fits 2 KiB, including its generic JSON child and
original traversal/metadata references. Its external input ring and
checkpoints keep the existing 8 KiB combined ceiling (6 KiB input and
2 KiB checkpoints). The original header/MIME storage
is borrowed through completion; this wrapper allocates no source, metadata
or preview collection and does not add a second native reader or snapshot.

Shared text eligibility examines one retained descriptor and its bounded
content-type evidence. Preparation preserves the original header-budget
check. Progress retains the generic preview's per-stage record and output
charges and at most six copied JSON bytes per poll; native freshness checks
add no replacement work allowance. The enclosing worker must account for all
retained backing and response staging separately.

## Native preview source selection storage

The selector fits 512 bytes, retains only original body-list/traversal
references, one choice and fixed cursor/credit state, and allocates nothing.
It needs no output backing. Each uncached turn charges one original job record
and one header interpretation step before checking a descriptor or list entry;
phase transitions have the same fixed charge. For P parts and T/H body-list
entries, healthy completion takes N = P + T + H + 3 charged turns and N
header interpretation steps. The selector's fresh header Credit prepays one
additional original Meter record per group of 16 steps, so total Meter
records are N + ceil(N / 16). No I/O or output bytes are charged. Ordinal bounds and strict increasing list checks bound
malformed lists as well as healthy ones; positive progress cannot exceed the
u16 part space per list. Empty and cached freshness operations spend no new
positive allowance. Enclosing admission owns all borrowed backing costs.

## Selected native preview composition storage

The selected-preview cursor fits 8 KiB including an inline union of metadata,
native preview and empty-source states. The union intentionally retains the
largest fixed variant without a heap box; stages reuse that same storage.
Caller-owned metadata and normalization backing retain their existing limits,
as do the input ring (6 KiB) and checkpoints (2 KiB), 8 KiB together. No source or projected
preview collection is allocated, and no snapshot or budget is replaced.

Selected-part interpretation and decoding retain their original charges. One
additional job record funds the bounded class and binary-search membership
checks on the selector-validated lists before decoder construction. The
empty-source branch spends exactly two job records and two output bytes, one
per quote, with no I/O or positive header steps. Empty output and cached
completion still check admission but spend no positive allowance. Outer worker
admission must reserve this cursor, all borrowed backing and response staging;
this component does not establish a whole-response memory bound.
## td-mail inbox query storage

Email/query's source owns one 64 KiB row buffer, a pending ID/date pair and
fixed arguments. Its 24-byte sort records contain an order-preserving signed
received timestamp and the Email ID; the shared native sorter retains at most
256 records and eight merge heads. The row buffer belongs to the existing
read-view row reservation; sort vectors use the existing exclusive 1 MiB
workspace reservation. No mailbox-sized vector of Email IDs is retained.

A source row read conservatively charges 64 KiB and one record before native
access, then yields. A matching-mailbox lookup charges 32 bytes and one record
on a separate turn, keeping each callback within the native sort-job ceiling.
Comparisons charge one record and inspect at most 24 bytes. Empty pages and
zero limits still scan/sort matches when needed to compute truthful totals.
One original sort-job meter covers all passes and output reads; exhaustion
returns an error without a new job or partial successful page.

Each nonzero page repeats the account-wide scan and matching-row sort. The
600-row fixture establishes ordering and paging, not large-mailbox performance.
At defaults the 64 KiB row charge alone bounds a scan below roughly 131000
account rows; native VM fuel and the two-million-record work grant can refuse
considerably earlier. Review estimated a dense mailbox near 17000 rows could
exhaust the sorter record grant; that is an unmeasured estimate, not a supported
mailbox-size promise. An indexed mailbox/date read is follow-up work before
large-mailbox qualification. A zero limit without a total skips source rows;
it still acquires the ordinary bounded view/sort reservation.


## td-mail Email/get composition

The body-job ledger now reserves configured header_bytes plus 512 bytes per
configured MIME part and 512 KiB of scratch. The per-part allowance covers the
original Part, optional header view, body-list Node, value-selection Fact,
locator Candidate and list/selection membership cells; a compiled assertion
bounds their combined representation. Part arrays are bounded by mime_parts,
not message byte length or account size.

Per-message storage includes one configured header arena; six metadata windows
of at most 16 KiB plus 14 bytes; one 192 KiB generated part fragment; 6 KiB input
and 2 KiB checkpoints; 3 KiB normalization scratch; and fixed parser/output
state. Metadata backing is reused through short part borrows; the fragment allocation is
reused for the next part. One 4 KiB staging buffer and 4 KiB source/output
buffer avoid per-scalar response file I/O. Requested stored rows reuse one
64 KiB method buffer in the read-view allowance. There is never a retained
collection of body strings or generated fragments for all parts/messages.

The same method meter survives all messages and transformations. Decoding,
preview extraction and JSON stages retain their original cumulative charges;
a small requested preview can still hit the work ceiling while validating a
large source. The response spool cap independently limits emitted bytes.
Default planned total is 78365952 bytes under the unchanged 128 MiB budget;
capacity-growth fixtures rebalance disposable cache within that budget.
These are bounded implementation allocations and planner figures, not a new
whole-service RSS or large-mailbox qualification claim.

The current 512 KiB scratch reservation contains the earlier components'
96 KiB base partition, including their named 16 KiB parser and 32 KiB
conversion regions. References to those original partitions below remain
component ceilings, not the current whole-job total. The per-part arrays now
have their separate 512-byte allowance; retained lists do not borrow the old
96 KiB partition. Header backing additionally reserves one lookahead byte per
part and two final bytes within that allowance and scratch.

The larger per-part ledger can reject an old high-part-count configuration:
with all other defaults, mime_parts above about 1900 no longer fits 128 MiB.
The scalar maximum remains 4096; raising it requires rebalancing another
reservation or increasing the admitted memory budget. Scalar maxima are not a
promise that their combination fits the same memory budget.

The reduced default headroom also affects previously admitted non-default
settings: with other defaults, smtp_recipients 465..1000, https_connections 9,
smtp_sessions 11..15, storage_views 6..8, or body_jobs 3..8 no longer fit the
128 MiB plan. Existing deployments with raised concurrency or object limits
must rebalance disposable cache or raise memory before this planner change.
The reservation applies at startup even while live Mail is not advertised.
CONFIG.md's resource validation uses this same combined plan; independent
scalar maxima do not establish admission. Selection additionally retains up
to 64 names (264 bytes each) and their parsed header properties, bounded by
the worker's parser/conversion scratch partition.

Direct-part downloads reuse the Email/get body-job reservation. They retain
one admitted header arena with a lookahead byte per part and two final bytes,
one Part and header View per admitted part, a 6 KiB input buffer, a 4 KiB
counting window and the existing HTTP output window. Locator matching produces
one temporary candidate at a time. No whole body or decoded attachment is
retained. Parsing, matching, counting and replay share one foreground meter;
the pinned native reader retains its independent storage work bound. Native
HTTPS coverage includes a 120,000-byte encoded part near the fixture admission maximum,
quoted-printable, empty parts and forged ranges/encoding/container requests.

The native td-mail read fixture exercises these workers serially through the
real fetch transport with four small messages, including an interpretation
refusal. It is functional evidence only. The single body worker and existing
request/authentication ceilings remain unchanged; this fixture makes no
latency, concurrent-reader or whole-service RSS qualification claim.

Shared-worker pressure can return 503, including at the boundary between API
reads and downloads while a previous exchange retires. For reads, the client
retries 429/503 at most twice using a one-to-five-second Retry-After, or one
second for 503 without that header. Persistent pressure remains an error.
No seamless concurrent-read or contention-retry guarantee is made here.

## Active HTTPS authentication worker

The current foreground profile adds one fixed `https-auth` worker alongside
its HTTPS handshake and body workers. It scans the existing bounded client
slots, with one lookup at a time and no separate queue or per-client thread.
Each slot retains one 352-byte decoded credential buffer plus its range and
one scalar authentication outcome. The request releases this storage before an ordinary response/EventSource or
on drop; Busy does not allocate another credential owner or renew attempt admission.

This is active runtime layout, not a claim that the proposed eight-role table
or 256-KiB worker-stack budget above is fully qualified. As with the existing
foreground workers, host default thread stacks remain outside that proposed
whole-process measurement. The isolated authentication worker keeps the
network/TLS/body workers responsive during bounded backend lookup. The HTTPS
fixture blocks the backend while a second client handshakes and receives its
origin refusal, then checks the original request resumes after local registry
contention without consuming a second attempt allowance.

Body-worker upload, download and API calls borrow that same credential owner;
they add no decoded copy or secret allocation. The owner is cleared as soon
as the synchronous backend call returns, before closing or draining the
connection. Initial lookup is still only an observation: storage authorization
and publication fencing remain the backend's responsibility.


The prepared relay greeting/EHLO owner retains its TLS connection inline.
With the standard boxed TLS wire reservations, its pending/ready progress
value has a compiled 4 KiB ceiling; the four caller-owned 512-byte control
cells and existing TLS wire/provider reservations are accounted separately.
The locally scoped large-enum lint allowance preserves inline custody without
allocating a box on each turn. Refusal destroys native TLS and the socket
before returning only the reusable input/output reservations.

The prepared plaintext relay greeting/EHLO owner borrows the same four
512-byte control cells and retains its socket and prepared TLS reservation
inline, with a compiled 4 KiB progress ceiling. Its opaque ready value releases
the EHLO cell borrows, allowing two cells to be reused by the STARTTLS/220
owner. Each turn performs at most one nonblocking socket operation and parses
one bounded control line under the original handshake deadline. It retains
the existing outbound slot and handshake permit; it adds no thread, socket,
provider buffer or per-turn allocation. Refusal aborts the socket and native
reservation before returning the reusable TLS wire buffers.

The prepared AUTH owner additionally borrows four 512-byte cells (input and
three distinct reply readers), a 1280-byte plaintext cell and a 1710-byte
encoded response cell. These 5038 bytes are reserved before the exchange.
Its standard boxed-TLS progress representation is compile-time capped at
8 KiB, separately from TLS reservations and borrowed cells. Encoding and
challenge decoding use the shared 256-transition codec turns; no turn grows
a collection or allocates. Every turn charges a record, pending included;
reads precharge 512 I/O bytes and writes precharge their remaining bounded
response extent before attempting transport. Codec input/output charges use
the original borrowed meter. These conservative reservations are consumed
on pending attempts too. Flush turns consume a record under the same fixed
deadline. Runtime scheduling must supply the admitted work budget and live
queue/route fences; protocol completion grants neither.

The prepared authenticated command exchange borrows three 512-byte cells
(input, command and parser line) plus one 4096-byte normalized reply cell.
Its standard boxed-TLS progress representation is capped at 4 KiB, separately
from the 5632 caller bytes and TLS reservations. Each turn performs at most
one TLS operation and parses at most 512 input bytes. The original borrowed
meter charges a record per turn, 512 I/O bytes before each read attempt and
the remaining command extent before each write attempt, including pending
I/O; pre/post checks retain the original absolute deadline. No per-turn
allocation or output growth occurs. The complete reply retains exclusive
connection custody until the worker copies its result and advances or aborts.

The prepared SMTP DATA encoder has a compile-time 96-byte state ceiling and
a two-byte pending output cell inside that state. Caller input/output buffers
and immutable-source custody are external. A poll performs at most 256 state
transitions and charges one record plus every consumed source/output byte to
the same original meter. Empty input/output supplies backpressure, and the
first refusal is sticky. No poll allocates or emits a terminator. Completion
and consuming finish freshly check the original meter/deadline; neither
constitutes a durable body/acceptance fence.

The prepared DATA body-transfer owner borrows a 4096-byte input cell and a
16384-byte encoded output cell, plus an immutable source reader and the original
meter. Its standard boxed-TLS progress value is compile-time capped at 4 KiB,
separately from those 20480 caller bytes and TLS reservations. A turn charges a
record and performs at most one source read, one 256-transition codec poll,
or one TLS operation. Source reads and TLS writes precharge their bounded
requested I/O extent; the codec separately charges consumed source and output
bytes. These conservative layer charges include pending TLS attempts. Clock
checks precede and follow every turn, including read, flush and completion.
No per-turn collection grows or heap allocation occurs in this owner.
Output accumulates across codec turns before one bounded TLS write; only
a full cell or completed body drains it. Partial writes retain exact offsets.

The prepared DNS wire helper keeps a canonical name in at most 258 bytes
and an address question in at most 264 bytes. Query encoding borrows at most
271 caller bytes and never changes bytes outside the returned query prefix;
short capacity writes nothing. Name decoding allocates no heap storage and
uses no recursion: at most 255 expanded name bytes and 32 backward pointer
hops bound one call. Packet input is at most 65535 bytes. These values belong
to the resolver's existing question/name/cursor partition, not one new
reservation per waiting lookup. Network work and its original deadline are
outside this pure wire helper.

The prepared DNS answer decoder uses at most 256 16-byte record descriptors
(4 KiB), nine fixed canonical-name slots for cycle detection, and a fixed
16-address result. The result retains those name slots as its complete local
path and is compile-time capped at 3 KiB; the decoder moves that same path
out rather than allocating another history. Cache entries retain only final
addresses and expiry, not this transient path.
These arrays belong to the existing resolver question/name/alias/result
partition; packet bytes remain borrowed from the 64 KiB packet reservation.
There is no heap allocation or recursion. One call frames at most 256 records
and scans the answer descriptors at most nine times; each name decode retains
the wire helper's expanded-byte and compression-hop limits. Network admission,
the original lookup deadline and cross-packet CNAME totals belong to the
resolver owner. The decoder enforces the supplied remaining alias allowance
(up to eight) on both incomplete chains and terminal address results; it
cannot reset or grant that allowance.

The prepared DNS transport borrows the existing 65535-byte packet cell,
keeps a 271-byte query and at most a 273-byte TCP query frame, and uses the
answer decoder's bounded arrays. It allocates no per-query heap storage and
has at most one socket live: UDP closes before TCP fallback. A lookup attempts
at most 16 UDP receives of 513 bytes; TCP reads use at most 16 KiB per call
under the original total deadline and work meter. Every socket I/O attempt
precharges a record and its requested byte extent, including repeated partial
operations; each query send also charges output bytes. Each parse reserves
2560 record units for one framing pass and up to nine answer scans, while
individual names retain their fixed byte/hop bounds. Capacity refusal and
clock expiry stop the exchange without another network attempt. These cells
belong to the existing fixed resolver reservation, not each waiting job.

The prepared DNS lookup owner keeps one fixed 16-address staging array and,
for each serial address family, at most nine traversed-name slots and bounded
name/question/result values. It reuses the transport's single packet cell.
Each family admits at most eight consumed aliases plus its terminal query;
each question tries at most four configured servers. Both families share the
original total deadline and sticky work meter. Boundary/entropy checks charge
record units in addition to the transport's I/O and decoder charges. No
per-query collection grows; successful output changes only its returned
prefix. Cache state is not allocated by this owner.

The configured service resolver uses two endpoint/cache slots (relay and
optional ACME), within the proposed 128-entry reservation. Each cache entry is
compile-time capped at 384 bytes and keys final addresses/expiry by endpoint
index and configuration generation. Names borrow the immutable configuration;
no CNAME history survives in cache. The owner borrows exactly the single
65535-byte packet cell and stores at most four numeric server endpoints. A
miss stages one fixed 16-address set and grants at most 4,000,000 record units,
64 MiB of attempted I/O and 1 MiB of query output under the original caller
deadline. These are work counters, not allocated storage. A hit performs only
bounded name/key/clock checks and a fixed address copy. No background refresh,
per-request thread or growing collection is introduced.

The prepared outbound dial helper borrows at most 16 numeric addresses and
holds at most one socket at a time. Each attempted address prepays two record
units for connect plus bounded nonblocking socket setup, including failed
attempts; it sends no application bytes. A two-second per-address connect cap
is further shortened by the original job and meter deadlines. It allocates no
per-address collection and hands success to the existing TcpTransport owner;
refusal drops any opened socket. Only the admitted sole outbound slot may
occupy a handshake worker for this blocking operation.

The prepared relay publication worker retains at most one uncertain operation
across attempt preparation and phase/result writes. A phase request retains
at most 100 ordinals and its existing bounded reply vectors; DATA-command
refusal may retain both RCPT and DATA replies, each capped at 4096 bytes per
recipient. It adds no second pending queue or connection pool. Phase work
conservatively prepays three MiB plus 128 KiB of protocol I/O and 4096 records
for receipt inspection plus batch publication; settlement uses the same
allowance without renewing the original job/meter deadline. The native phase
routine shares the immutable request through Arc; bounded SQL rows remain
charged separately from that retained request. This is process work admission,
not a database disk/WAL reservation or the final-outcome capacity for SMTP handoff.

PostgreSQL final-result claims retain at most 1024 account rows, each holding
one attempt key and at most 400 ordinal bytes. Under the account lock they
preserve one sequence credit and a slot in each of four alternative receipt
quotas. Allocation/consumption uses bounded counts and at most one selected
100-recipient subset; expiry decodes at most 400 bytes into 100 ordinals.
The phase/recovery/expiry transactions retain their existing connection byte,
frame, reply and deadline bounds. These durable logical claims do not reserve
physical database disk/WAL or the relay's future local connection, work and
reply storage. Those local reservations remain required before SMTP handoff.

The prepared FinalWork owner retains one ordinary publication ReadPermit and
prepays the existing 3 MiB plus 128 KiB phase I/O ceiling and 4096 records.
Its original deadline is fixed at admission, including the ten-second native
operation cap. The owner contains references, one attempt ID, one Instant and
one permit; it allocates no queue or reply storage. Concurrent retained owners
are bounded by the existing slot pool. Consumption neither reacquires a slot
nor charges again; drop returns only the slot. The selected phase request and
SQL rows retain their separate memory bounds;
native receipt/publication owners share that immutable request through Arc.
This is only the connection/work part of local final-outcome capacity.

FinalReply preallocates at most 100 reply strings of 4096-byte capacity
(409600 bytes), alongside the existing 100-entry ordinal/string vectors and
bounded claim metadata. It retains two inline SHA-256 prefix states. Finishing
fills only those retained buffers and moves the already allocated immutable
request owner; native phase/receipt copies share its Arc. The sole admitted
outbound job owns this storage separately from its fixed SMTP/TLS buffers and
FinalWork connection/record/I/O allowance. No extra pending queue is added.

The pending operation owner boxes its existing bounded Dispatch during cold
attempt preparation; a phase variant holds only the shared Change owner.

CommandReply reserves the same at-most-100 4096-byte negative reply cells
before MAIL or DATA-command transmission. DATA additionally retains at most
100 positive RCPT replies of at most 4096 bytes each: 819200 reply bytes
across both vectors, plus bounded ordinal/string vectors, claim metadata and
two inline hash states. MAIL fills one reply vector and initializes only the phase hash state.
Finishing fills retained cells and moves the existing Change owner; it does
not allocate new reply storage. The sole outbound job owns this allowance
separately from SMTP/TLS buffers and native database work. This passive
reservation holds no connection permit and creates no additional pending
queue or publication deadline.

DataPlan retains both alternative DATA-command requests before transmission:
the Body request's positive RCPT vector beside CommandReply's positive RCPT
and reserved negative vectors. At 100 recipients with 4096-byte replies this
is at most 1228800 reply bytes, plus bounded vectors, claim/envelope metadata
and inline hash states. Consuming either result drops the unused alternative;
no new reply-vector allocation is needed after actual 354 or refusal. Native
database allocations and work admission remain separate.

RcptReplies allocates at most 100 reply cells of 4096 bytes before any RCPT
command, or 409600 reply bytes total. Recording moves cells from unused to
positive or negative vectors; these three sets partition the original cells.
Bounded ordinal and result vectors, claim metadata and one retained hash
context are additional. Finish drops unused cells and creates no reply copies.
The resulting refusal uses the existing phase publisher's separate database
memory and admission bounds; positive evidence is passive retained text.

Current-owner interruption may retain 100 positive RCPT replies of 4096 bytes,
or 409600 reply bytes plus ordered ordinals and operation metadata. Recovery
clones this bounded request for one native operation; the caller and operation
can therefore retain two such vectors concurrently. Its existing batch client
admits one MiB of output for the parameter arrays and protocol overhead, with
the unchanged one-MiB/1088-frame input budget and ten-second job deadline.

Recovery publication uses the existing PHASE_IO allowance (3 MiB + 128 KiB)
and 4096 records before acquiring one ordinary connection slot. Unknown
COMMIT retains one boxed Recovery in the worker's existing Pending owner;
settlement uses independently provisioned slots and the same conservative
work allowance. At the maximum positive-RCPT subset, a caller retry copy,
Pending request and native operation clone can hold three 409600-byte reply
vectors concurrently, plus bounded metadata and existing protocol buffers.
No second queue, wait list or transport owner is created.

RcptPlan retains the existing envelope and at most 409600 RCPT reply bytes.
Planning DATA temporarily retains that collection beside the new Body and
CommandReply alternatives: at most four 100-by-4096 vectors, or 1638400 reply
bytes. The source positive cells drop on successful handoff; the resulting
DATA/negative-RCPT requests contain at most 1228800 reply bytes. Interruption
planning temporarily retains the source cells and a copied positive subset,
at most 819200 reply bytes; its resulting refusal/recovery requests together
contain at most 409600 reply bytes. Bounded borrowed-reply vectors, envelope,
claim and hash metadata are additional. Planning failures drop incomplete
alternatives and retain the original collection. Native publication copies
and protocol buffers retain their separately documented bounds.

Mixed negative-DATA settlement retains at most the DATA CommandReply's two
100-by-4096 vectors and the disjoint negative-RCPT cells, bounded by 819200
reply bytes. Capturing the actual DATA reply drops the unused Body alternative;
retained failures and Pending phase references share immutable Change storage.
The envelope box was reserved before RCPT, and no new reply vector is allocated
between capture and database admission. Metadata and native protocol copies
retain their separate bounds. Each attempted publication charges PHASE_IO and
4096 records; successful preceding RCPT work is not repeated on a known retry.

Passive command-evidence extraction moves existing Change references without
allocating or copying reply cells. It drops the retained worker/deadline owner;
the shared Pending operation, if present, remains separately owned. New
settlement uses ordinary PHASE_IO/4096-record admission and connection slots
for each publication; it retains no SMTP slot or transport authority.

Body-aware interrupted RCPT evidence uses the existing at-most-100 positive
cells and 409600-byte request bound. Its mode and domain-separated digest add
constant metadata only. Publication counts Prepared updates separately and
verifies Body cells in the existing bounded locked-row batch; it adds no query,
reply vector, connection slot or transport owner.

Bound positive-DATA publication retains the existing boxed DATA alternatives
and optional negative-RCPT Change. The box is allocated before DATA and failures
move it without post-reply allocation. Fenced interruption planning may overlap
the three DATA reply vectors, disjoint negative-RCPT cells, and one copied
positive recovery vector, at most 1638400 reply bytes plus existing bounded
metadata and borrowed references. Successful conversion drops DATA alternatives;
negative/recovery requests together retain at most 409600 reply bytes. Native
publication copies and protocol buffers keep their separate bounds.

Final evidence extraction moves the existing Change without new reply storage
and releases its local ReadPermit. Pending may retain a shared reference to the
same immutable request. Fresh passive publication charges PHASE_IO/4096 records
and uses an ordinary slot; independent uncertain-COMMIT settlement retains its
separate slot budget. The durable final-result capacity claim is unchanged by
extraction and remains charged until its existing terminal transaction consumes it.

The native MAIL adapter owns its existing MailPlan and inline TLS exchange,
borrowing command input/line/reply/output cells and both work meters. It adds no
reply-vector copy, per-turn wrapper allocation, connection reservation or
background queue. Live observation retains its existing native allocation
bounds. Each write/flush observation charges ATTEMPT_IO/4096 records and acquires
an ordinary slot only for the database operation. Negative publication retains
the existing preallocated MAIL evidence and PHASE_IO charge; all known and
uncertain failure custody follows the existing CommandFailure/Pending bounds.

The ordered RCPT adapter adds no reply copy to RcptPlan's preallocated maximum
batch cells. A command owns inline TLS/parser state and borrowed scratch/meters;
its non-borrowing Cursor/Completed handoff releases scratch for the next command.
No extra command queue, connection reservation or per-turn wrapper allocation is
introduced. Existing envelope observations retain their native allocation and
ATTEMPT_IO/4096-record charges. Finishing moves cells into existing RcptResult;
passive negative/recovery planning and publication retain their established
reply-vector bounds and fresh admission charges.

The bound DATA adapter owns the existing boxed RcptDataPlan and inline TLS
command exchange, borrowing parser cells and both work meters. It introduces no
reply-vector copy, queue, per-turn wrapper allocation or connection reservation.
Fencing a negative reply aborts TLS while retaining already-normalized parser
storage; the existing TLS buffers and policy slot remain owned until returned
after bounded publication. Pre-output checks retain ATTEMPT_IO/4096 charges;
ordered result publication retains PHASE_IO and existing mixed-RCPT/Body failure
reply-vector bounds. Refusals move existing passive ownership into a typed enum.

A command refusal after complete parsing may keep a code and borrowed view
of the existing normalized reply cell alongside closed TLS buffers. Its
standard boxed-TLS representation is compile-time capped at 128 bytes. It makes no
reply copy, reserves no additional cell and cannot return a session. The cells
remain borrowed until refusal custody is consumed; callers capture evidence
before extracting buffers. Adapter publication uses its already reserved reply
storage and original meters. Partial or malformed input retains no result.

The bound DATA-to-body handoff moves its existing BodyResult, Dispatch, inline
session and MAIL mode into BodyReady after existing final-capacity reservation.
It adds no reply copy, protocol buffer, heap allocation or connection. Its
BodyPlan retains the existing final-result ReadPermit; live checks need their
ordinary additional slot. Reservation failure closes transport and releases
local custody, while existing durable phase/recovery rules remain in force.
Immutable source verification and replay retain their separate bounds.

Live Body validation sends at most 100 32-byte RCPT reply witnesses plus bounded
row metadata, instead of retaining up to 100 full RCPT and DATA strings in
native result rows. Each local expected digest uses the existing SHA-256
provider and one transient 32-byte result; no digest vector survives the check.
The fresh connection retains 1 MiB input and 1088 frames, with 64 KiB output,
including authentication and runtime qualification. Body-check admission
charges 1 MiB plus 128 KiB and 4096 records, preserving a 64 KiB margin over
the protocol byte ceilings. Publication, Possible validation and final-capacity
reservation keep their existing charges; all slot and shrinking-deadline
rules are unchanged.

The standalone BodyChecks connection retains its 1 MiB input, 1088-frame and
64 KiB output ceilings over every validation turn. It shares the existing
immutable Change through Arc and owns one worker plus clock/guard handles;
no per-turn reply vector is retained. Its thread-bound owner closes on any
refusal, deadline or cleanup failure. Each successful turn acknowledges
rollback of transaction-scoped migration and row locks before returning.
Standalone dispatch checks charge one fresh connection per Body check;
retaining the low-level validator supplies no slot or work admission.

The bound Body transport prepays source-sized validation work before opening
one retained ordinary connection. For actual verified length L <= 32 MiB,
E = L + floor(L/3) bounds encoded output. The check count is 2 * ceil(E/16384)
plus one flush and eight extra partial/Pending I/O attempts. Each cell
reserves acceptance and a TLS-record tail drain under the 16 KiB socket cap. Each check
prepays 1 MiB plus 128 KiB I/O and 4096 records; checked arithmetic derives
cumulative 1 MiB input, 64 KiB output and 1088 frames per admitted check.
At 32 MiB this is 5471 checks, 6154.875 MiB I/O, 22409216 records, 5471 MiB
input, 341.9375 MiB output and 5952448 frames. These are work counters, not
memory allocations, and make no maximum-message throughput guarantee.

Each turn independently permits at most 1 MiB input, 64 KiB output and 1088
frames. Authentication consumes the first window; available idle traffic
consumes the preceding window. Only acknowledged rollback permits a later
window. Its finite grant count matches the source-derived check count;
protocol failure permanently refuses further grants. Aggregate counters
continue decreasing across all windows. The separate dispatch credit counter
also consumes Pending writes and flushes. No aggregate counter, credit or
meter is replenished. Every turn checks Pending and the original shrinking
deadline and performs full qualification plus live validation. Earliest
limits carry into Possible and final custody. Connection closure precedes
ReadPermit release, including before Possible needs the second slot.

Bound immutable-source admission prepays 131 MiB I/O and 32768 records before
opening the snapshot or either fresh attempt admission. This covers their
130.625 MiB combined protocol ceilings with 384 KiB margin and does not reserve
that much memory. The existing 512 KiB verified-body cache and frame bounds
remain unchanged. Two permits from an independent source pool cover the
snapshot/second-admission overlap; aliasing the ordinary pool refuses.
Both connections close before source permits return. The original ordinary
two-slot pool separately covers retained validation and final custody.
The callback-scoped owner adds no body copy or protocol buffer; it retains
the existing DATA owner, verified-reader borrow and private completion-sink
borrow. Scoped progress exposes no final authority. Its callback result and
private completion stay outside the snapshot's final check so failure can
close Completed explicitly and preserve an existing transfer refusal. The earliest source admission
limit carries through Body and final custody without extension.

A prepared native relay Binding retains one optional TlsPolicyId from its
same cold-loaded TLS generation. Inspecting the unpublished generation exposes
only its ID and borrowed payload; the Binding owns no lease, credential copy
or extra TLS allocation. Claim/Dispatch clones retain this fixed passive identity. MAIL
compares it before command construction without I/O, allocation or new work
admission. Existing live database and wire checks retain their original cost.

FailedPrepared retains bounded passive custody for one Prepared attempt.
Cold preparation derives one health change and one empty-positive-reply
Recovery from the retained Dispatch. Its health stage holds both immutable
requests, with the health change boxed once during preparation; its recovery stage retains only Recovery and the fixed health
outcome. Unknown publication additionally owns one boxed request in the
existing Pending slot; native inspection/publication may clone that bounded
request. It allocates no message-sized body or reply collection.
Each publication and inspection prepays PHASE_IO (3 MiB + 128 KiB) and 4096
records before connecting, using the supplied/meter deadline and ten-second
cap. Publication acquires one ordinary slot; inspection acquires one slot
from a separate settlement pool. No slot survives a step. Historical
reinspection does not renew SMTP work or bypass an outstanding Pending
operation. Completed outcomes are fixed-size and return without I/O.


The inert native relay connector reuses MailPlan's envelope/reply reservation,
retains one private passive Dispatch and one same-load PublishedProfile lease,
and borrows five 512-byte control cells, the existing AUTH plain/encoded cells,
and one resolver packet. A 255-byte local hostname cell crosses the temporary
configuration graph view; addresses use the fixed sixteen-address DNS array.
ConnectedPrepared has an 8 KiB compiled ceiling with boxed TLS wire arrays;
its passive Closed result has a 2 KiB ceiling. Cold publication returns the
original preparation inline on refusal; setup failures retain passive owners
inline rather than allocating failure boxes. No body storage is added.

Each unmetered plaintext greeting/EHLO/STARTTLS turn prepays one record and
512 I/O bytes. Each TLS handshake or encrypted EHLO turn prepays one record
and twice TLS_WIRE_BYTES, covering one inbound and one outbound record attempt.
These are conservative work reservations, not measured traffic. Pending
consumes its reservation; the same wire meter survives all setup transitions.
AUTH retains its own per-turn/protocol accounting without a second outer
charge. Pending polling sleeps for at most five milliseconds or the remaining
cap, then checks the cap again. Work exhaustion remains local Capacity.
Configured DNS keeps its existing separate per-cache-miss 64 MiB I/O,
four-million-record and 1 MiB output allowance under that same time cap.
Numeric dialing retains its two-record/address charge and two-second/address
ceiling. Fresh native envelope checks keep their existing ATTEMPT_IO and
4096-record precharge; no slot survives into socket work. The connector runs
on the caller's admitted worker and creates no thread or queue.


The inert single-attempt driver retains one bounded passive Dispatch clone
before consuming ConnectedPrepared. Its terminal enum owns the existing
phase-specific evidence inline and has a 16 KiB compiled ceiling with boxed
TLS wire arrays. It introduces no body copy or per-command reply allocation;
existing envelope, reply, source and final reservations remain responsible
for their storage. Command/body scratch is borrowed and reused only after
the preceding owner releases it.

Wire and database meters span the entire attempt without replenishment.
Immutable source work and two independent source slots remain explicit.
Existing advances charge their work; the driver adds no second charge.
Pending commands and repeated body Write/Flush turns wait at most five
milliseconds under the retained upper cap. Source reads and codec turns
with runnable work proceed without that polling delay. The phase hint
borrows existing state and adds no storage. Each advance still checks its
possibly tighter deadline before work.
This bounded driver does not qualify maximum-body throughput or arbitrary
backpressure. Source-sized Body admission still refuses on finite credit,
work or original deadline exhaustion.

MAIL/RCPT/DATA command owners carry one private per-turn transport-progress
boolean. Validated positive-length command writes or reply reads, and
completed flushes, allow immediate bounded continuation. Each advance resets
the hint, so Pending operations still wait. Parsing retains its 512-byte/
one-TLS-operation turn and original meter/deadline checks. Fresh database
authority checks before writes/flushes and their finite prepaid turn counts
are unchanged. The configured retained-envelope fixture below covers the
combined path; final DATA terminator pacing remains unchanged.


Standalone passive EnvelopeChecks::open uses the existing fixed
1 MiB input, 1088-frame and 64 KiB output connection profile. All counters
span authentication and successive fresh validation transactions without
refill. Each check reuses the standalone bounded envelope parser and returns
at most 100 recipient observations. The original deadline only tightens;
any validation, protocol, cleanup or expiry failure permanently drops the
backend. The owner remains thread-bound and supplies no connection permit
or work budget itself. Native fixtures verify the aggregate limits and
lock release. Configured SMTP admission is accounted for below.


Configured MAIL entry prepays retained envelope checks for 2*(N+2)+8 turns,
where N is the admitted 1..=100 recipient subset. The shared validation budget
charges each turn 1 MiB+128 KiB I/O and 4096 records and limits its PostgreSQL
protocol to 1 MiB input, 64 KiB output and 1088 frames. Authentication shares
the first window; idle messages consume the preceding window. At 100
recipients, 212 turns charge 238.5 MiB I/O and 868352 records, with aggregate
212 MiB input, 13.25 MiB output and 230656 frames. These counters do not
allocate those byte totals. No turn, counter, deadline or meter replenishes.

One ordinary ReadPermit covers the thread-bound envelope validator through
configured MAIL/RCPT/DATA. Its inline custody has a compiled 1 KiB ceiling;
protocol/provider allocations remain separately bounded. Every fresh
transaction acknowledges rollback
before transport; only the connection survives. Its connection closes before
permit release and before negative/positive DATA publication, Body/source
admission or passive evidence handoff. Original Pending, identity, policy,
ownership, clock and deadline checks remain mandatory on every output turn.
Direct low-level envelope observations retain their existing per-call profile.


Route-candidate pages through Dispatches prepay 1 MiB+128 KiB I/O and 4096
records and hold one ordinary read slot for the bounded query. The existing
mailbox protocol profile bounds input to 1 MiB, output to 64 KiB and Runtime
frames to 1024; at most 64 returned hints survive. Original input/meter and
ten-second operation caps apply, with no waiting queue or retained connection.
The shared Pending guard excludes another dispatch operation for that read;
receipt settlement remains on independently admitted capacity. Candidate
observations do not change queue-clock admission or grant SMTP permission.


Worker route acquisition prepays 2 MiB+192 KiB I/O and 4096 records across
one receipt connection and at most one mutation connection, using one ordinary
slot sequentially. Their existing protocol ceilings remain 1 MiB input per
connection, 64/128 KiB output and Runtime 1024/1088 frames respectively.
The shared Pending enum retains one bounded Claim only after admission; its
identity distinguishes attempt, route-claim and expiry operations. Settlement
prepays the same conservative work bound on independently supplied slots,
performs receipt inspection only and retains the claim on absence or error.
No waiting queue, connection retention or transport authority is added.

The admitted queue-expiry worker uses the same two-connection conservative
prepayment as route acquisition: two MiB input plus 192 KiB output and 4096
records. Receipt inspection and possible publication run sequentially under
one ordinary permit and the original deadline capped at ten seconds. Their
Runtime frame ceilings remain 1024 and 1088. The exact bounded Expiry occupies
one shared Pending slot after admission; unknown COMMIT retains it until
inspect-only settlement finds its receipt. Settlement uses independently
reserved capacity, conservatively prepays the same work and never renews the
original publication or SMTP budget. An expiry outcome is historical only.
The existing confirmed clock guard, spacing and 100-recipient transaction
limit remain mandatory; this admission layer neither confirms the clock nor
allocates an unbounded queue of expiry operations.

Administrative PostgreSQL device listing fetches at most 257 rows and returns
at most the existing 256-device account ceiling, including revoked records.
Only ID, label and two timestamps are selected; the 128-byte label bound makes
the command's complete escaped JSON output less than 128 KiB. Overflow and
invalid metadata return no partial listing. One mailbox-profile connection
caps input at one MiB, output at 64 KiB and frames at 512 in Preparation mode
(1024 in Runtime mode). The original deadline remains capped at ten seconds;
the preparation command includes fresh DNS in that same operation deadline.
A successful snapshot acknowledges rollback; every connection closes before
command output. No
connection pool, response spool, retained credential or serving authority is
created by enumeration.

### PostgreSQL usage-counter audit

The explicit historical account usage audit uses the ordinary owner-side
probe: one connection, a ten-second absolute deadline, 128 inbound frames,
one MiB total plaintext input and 64 KiB output. Queries return at most two
account/usage rows and one aggregate row; no body or metadata collection is
materialized by the client. Five fixed counters and one view identity form
the result. Server statement timeouts bound the aggregate scans;
numeric overflow and any deadline or query failure refuse rather than return
a partial report. Read-only snapshot rollback and the final clock check must
finish before success. No runtime pool reservation or full semantic-check
coverage is claimed by this administrative preparation operation.

## PostgreSQL single-body audit

The cold administrative `postgres_blob_audit` component reuses the bounded
body worker and verifier: at most 32 MiB of body data per invocation, read in
batches of up to eight 64-KiB chunks under one original finite connection
budget and deadline. Manifest selection returns at most two rows. It retains
one chunk batch, digest state and public metadata; no complete message buffer
or body bytes escape. Success follows acknowledged snapshot rollback and a
final deadline check. This is historical single-body verification, not whole
store verification, resource admission or restore authority.

## PostgreSQL canonical metadata audit

The owner-side metadata sweep uses one existing body worker: 128 MiB total
plaintext input, 8192 inbound frames, 512 KiB output and the original deadline
capped at ten seconds. No limit renews between lookups. One keyset/exact query
retains at most one canonical SQL row. An Email lookup also retains at most
1001 receipt rows (at most 254 address bytes each); shared receipt encoding
refuses beyond 1000 recipients or 32768 encoded bytes. The client additionally
holds one shared MAX_VALUE_BYTES buffer, a MAX_KEY_BYTES buffer, fixed sweep
state and a 32768-byte receipt encoding buffer. Parameter/key strings are
bounded by canonical key limits. No account-sized collection is retained.

Caller row bounds apply independently to each shared pass; parent reads have
one separate bound. SQL statements retain their finite server timeout. Wire
and deadline exhaustion can refuse before logical limits; success requires
all shared passes, acknowledged rollback and the final deadline check. The
report covers canonical field/reference, mailbox-chain and recipient-coverage
checks only, with the exclusions in POSTGRES section 202.

## PostgreSQL composed account audit

The combined account audit shares one existing body-worker connection across
metadata, counter aggregation and body enumeration. Its 128-MiB input,
8192-frame, 512-KiB output and original ten-second deadline limits do not
reset between components or blobs. Metadata memory is as specified above;
after that pass releases its buffers, one manifest and one eight-chunk body
batch are retained at a time. Caller aggregate count/byte bounds are checked
before each body's verification; each body still has the 32-MiB maximum.
Counters, captured identity and historical totals are constant-size state.
Mailbox-role uniqueness adds one account-scoped aggregate returning one
boolean within this same snapshot and original protocol/deadline budget.
The existing statement timeout bounds server grouping; the client retains no
role list or account-sized collection.

The explicit limits can be larger than the finite worker can finish. Such an
account refuses without a partial report. Every pass, body digest, aggregate
comparison, acknowledged rollback and final deadline check must complete;
there is no continuation or maintenance-time extension in this increment.

## PostgreSQL runtime maintenance refusal

Common runtime qualification adds one query returning at most two rows of two
booleans. The same connection input/frame/output and original deadline limits
cover it; no budget is renewed. It retains no barrier cache or permit. Missing,
blocked or unreadable state refuses qualification; existing finite operations
and external network fencing remain separate, as specified in POSTGRES 205.

## PostgreSQL owner maintenance entry

Owner inspection and entry use the existing schema worker's one-MiB input,
1024-frame and 64-KiB output limits with the original ten-second maximum
deadline. The bounded migration ledger, one owner-check row and at most two
barrier rows are the only materialized results. No budget resets, queues,
background tasks or barrier permits are introduced. Entry uses a nonwaiting
exclusive advisory lock and one synchronous commit. The transaction's idle
timeout remains ten seconds; existing statement and transport bounds apply.
Inspection acknowledges read-only rollback and checks the final deadline.
Mutation uncertainty and the same-epoch retry contract are in POSTGRES 206.

## PostgreSQL retained-history audit

The inert canonical history audit retains at most 128 native change rows,
one cursor and scalar counts. It uses the existing body-worker 128-MiB input,
8192-frame and 512-KiB output ceilings and original ten-second deadline,
including account capture, all keyset reads and acknowledged rollback.
Each accepted row spends the explicit total row allowance; failure returns
no partial report. Retired rows count against that allowance. No full history
vector, live-object index, body buffer or new worker budget is introduced.


## Active PostgreSQL relay worker

A relay role adds one SMTP thread and one lease-renewal thread, independently
of the fixed receiving/HTTPS workers. The relay owns two reusable TLS wire
buffers, fixed command/reply/body/DNS scratch, one bounded candidate page,
one active attempt and at most one retained failure continuation. It reserves
two ordinary PostgreSQL read slots, two independent body-source slots and
one separate settlement slot. Renewal
uses a separate bounded database call and cannot consume a body-source slot.
POSTGRES.md section 212 owns the work-meter ceilings and lifecycle. These
explicit active-worker reservations are additional to the planned resource
ledger; whole-process RSS and maximum-body throughput remain unqualified.


The relay's local-notice turn additionally retains at most 10 MiB of report
text, 16 MiB of encoded MIME and one 100-recipient SQL page. It runs serially
with SMTP attempts using one ordinary slot, with a five-second deadline and
at most one turn every five seconds. POSTGRES.md section 213 records its
separate aggregate database framing and composition work ceilings. The
shared account body/blob quotas include generated notices. Failure retains
only the database Pending marker; no unbounded local retry queue is added.


## PostgreSQL attachment composition

The single HTTP draft owner additionally retains at most 64 source
manifests/descriptors and an aggregate 32 MiB of verified raw attachments,
bounded further by the profile's message ceiling. Composition retains at
most another 32 MiB of message bytes; the existing growable composed buffer
may reserve capacity above its current length. Raw sources are released
before database publication or uncertain-draft retention. Each source read
reserves two ordinary database slots for its verified snapshot and fresh
primary authorization; the existing bounded body cache/protocol buffers
remain additional. No new thread, unbounded queue or connection pool is added.

All reads share the original request meter and ten-second-or-shorter deadline.
Before each source/address attempt the owner prepays 192 KiB plus three times
the remaining aggregate source allowance in I/O, and 1024 plus three times
ceil(remaining/4096) records. This conservative charge covers verification,
replay and framing even on fallback. Composition and source/body digests
charge the same meter separately. Refusal discards provisional bytes; the
existing pending owner retains only the prepared message/manifests after an
unknown COMMIT. Filename, media type and description limits are 255, 127 and
1024 bytes per part; composition uses fixed encoder/header scratch. This is
a finite memory/work bound, not maximum-size throughput qualification.
