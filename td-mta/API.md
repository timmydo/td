# Store and runtime boundaries

This normative M02c2 contract accompanies the compiling interfaces in
src/ports.rs and the state codecs in src/sync.rs. Interfaces do not supply a
store, network runtime, cryptographic provider or allocation/durability proof.
M05/M07–M09 implement and test the adapters; M13–M17 implement their protocol
consumers. RESOURCES.md fixes worker/buffer ownership; ADMISSION.md fixes
disk/work ledgers and exact request retention before consumers.

## 1. Ownership and I/O

Handles lease startup pools and release capacity on Drop; they cannot grow
collections per operation. Errors are fixed typed values, with OS kind/code
where relevant, never dynamically formatted diagnostics. Implementations may
use cold allocations only where RESOURCES.md budgets them. No trait call
authorizes access merely because it accepts a typed ID. The coordinator
checks account/device authorization and rechecks it at the operation boundary.

Clock returns UTC plus monotonic boot-local milliseconds. Deadline uses the
latter, checked addition and expiry at now >= deadline. Never persist Tick
or compare it across boots. Shared cryptographic representations, entropy
failure handling and constant-time requirements belong to td-crypto/DESIGN.md;
section 1.1 below defines the mail integration. TLS owns peer certificate
verification. Neither fake digests nor trait signatures prove crypto quality.

Transport methods are nonblocking and make bounded progress. Bytes(n) means
1 <= n <= supplied slice length; an empty slice returns Pending. Pending
does not consume bytes, and Closed on read is EOF. A failed/closed write is
an Error. flush drains all adapter-owned output, including TLS records,
through Pending to Complete; completion does not prove peer receipt. Drivers
flush before waiting for a reply and continue progress within the deadline.
close progresses close_notify through Pending/Complete and is idempotent
after completion. abort immediately fences I/O, closes the socket and discards
buffered output; failed/canceled attempts use abort. Caller buffers are not
retained after a call. TLS can buffer consumed plaintext, so queue
phase fences precede write calls, not socket flushes. Handshake Pending owns
one bounded handshake slot until completion/failure/deadline. No application
bytes are consumed before handshake success. TLS info remains tied to that
connection and verified policy: None for ordinary inbound TLS, ServerName
only after chain/time/name validation, Gateway only after the configured
gateway certificate identity check. Gateway bytes are SHA-256 of leaf DER;
authorization must also match configured gateway policy. A certificate alone
never enables arbitrary SMTP relay. Endpoint creation/configuration is a
cold runtime concern, not caller-supplied network input to these traits.

The concrete TLS factory uses three stages: reserve_session retains policy,
handshake capacity and wire buffers; construct creates native state on a TLS
worker; SessionReservation::handoff consumes TCP and returns TlsConnection.
It is used immediately for implicit TLS and after the exact
STARTTLS exchange for SMTP. Before replying 220, reserve the upgrade capacity;
flush that reply completely before the handoff. Reject/abort if any plaintext
bytes remain after the STARTTLS command or outbound 220 response. Never carry
pipelined plaintext into TLS parsing or discard a tail and continue. After
handshake success reset the SMTP parser, EHLO/extensions and authentication
state. Refusal/timeout aborts the consumed connection with no downgrade.
The supplied policy must still be a retained, authorized generation. These
operations reuse the same transport slot and add no unbudgeted connection.

Resolver runs on the one fixed resolver worker, accepts a configured hostname
and an absolute deadline, and writes addresses into caller capacity. Success
has count > 0 and <= output.len(), plus monotonic TTL expiry. Oversized answer
sets fail Capacity; no partial successful address list or allocating NSS
fallback. Implementations must stop at the deadline and validate DNS source,
question, name/compression limits and CNAME chains (M09). This port resolves
configured A/AAAA endpoints only; MX/TXT/policy resolution is outside v1.

### 1.1 Crypto provider boundary

Under DESIGN section 3's dependency boundary, `ports` re-exports td-crypto's
Crypto, Digest and Entropy traits and its Error as CryptoError. Their methods
return that shared error. The total From conversion into the mail Error enum
preserves Capacity, Invalid, Entropy and Crypto without provider strings or
secrets. Existing mail callers can use
`?` across this boundary. Clock and mail/TLS transport policy remain local.

The shared contract and implementation rules live in
[td-crypto/DESIGN.md](../td-crypto/DESIGN.md), with the TLS operation
contract in [td-crypto/TLS.md](../td-crypto/TLS.md). Its implemented Crypto
factory, SHA-256, worker-local entropy and P-256 key operations use opaque td-owned
handles. Direct streaming SHA-256 uses owned inline state; other implemented
operations retain the private AWS-LC backend. Opaque client/server TLS sessions
are implemented; service/resource qualification remains M07 work. Shared backend conformance
fixtures live in td-crypto; service integration tests reach it through the same
public facade as production.

Mail transport adapters implement the staged factory and TlsTransport using
td-crypto's opaque configuration/session interfaces. td-mta owns
policy-generation/slot leases, sockets, deadlines and STARTTLS
handoff/reset. td-crypto performs TLS handshakes and generic certificate
validation; the mail adapter additionally checks gateway allowlist policy
before constructing Gateway proof. A raw peer certificate or digest is not
proof of that authorization. HTTPS/ACME and the smart-host client use this
same boundary. Map shared TLS progress into the existing transport progress,
including Pending for empty caller slices. The adapter assembles one bounded
wire record, retains short socket-write tails, reports whether such a tail
remains to each shared record-intake call, and distinguishes read EOF from
write closure. It maps terminal facade errors to the mail Tls error, while
its own lease/deadline failures keep their existing mail error. Any failed
handshake or session aborts its socket. Shared TLS clock seconds come from
checked conversion of the injected mail UTC milliseconds; negative time or
Clock failure is unavailable time, never a system fallback.

Shared configurations/keys and their backend allocations count in the existing
RESOURCES.md generations and leases, including cold overlap. Moving their code
to another crate adds no memory allowance. M07 implements the shared crate's
conformance/qualification requirements and this file's transport matrix; F04
repeats them with bidirectional key/rollback fixtures before backend replacement.
Standard persisted formats remain stable. Provider process failures described
in the shared design follow the service's crash/durable-recovery contract.
Neither interface tests nor fake digests prove a cryptographic implementation,
bounded TLS behavior or resource limits.

### 1.2 Implemented socket and clock foundations

M07d1 supplies `transport::TcpTransport`, consuming one already connected,
exclusively owned standard TCP stream. It sets nonblocking mode and TCP_NODELAY,
clears socket timeouts and captures the actual socket peer before publication. Its caller
must supply an ordinary socket without positive linger or another live handle;
the adapter exposes no cloning, raw descriptor or stream extraction. Slot
admission, listeners, DNS, dialing and protocol deadlines remain runtime work.
No service entry point constructs it yet.

Each read/write attempts at most one data I/O call over at most 16 KiB;
terminal errors additionally shut down and close the stream. TCP_NODELAY
prevents Nagle buffering of short TLS-record tails after a full-size chunk.
WouldBlock and Interrupted return Pending without an internal retry loop.
EOF closes only the read half; orderly close shuts down only the write half
and remains idempotent, allowing the peer's final data to be read. A nonempty
write after local close is a terminal BrokenPipe error. Other terminal I/O
errors retain their first fixed kind/code, shut down both halves and drop the
stream; later nonempty I/O, flush and close return that same error. Abort uses
Invalid when no earlier error exists. Empty reads/writes always return Pending,
including after failure; they grant no progress. Drop aborts the stream.
There is no userspace output buffer, so flush completes immediately on a live
stream. Flush/abort cannot establish peer receipt or retract bytes already
accepted by the kernel. This is not a promise of TCP reset delivery.
Terminating on a write error deliberately discards any unread peer reply.
The protocol reports a transport failure rather than reconstructing a reply
after cancellation. Queue handling must retain an already established final
DATA acceptance uncertainty under QUEUE.md; the absence of a parsed reply is
not evidence that a remote delivery failed or is safe to retry.

`clock::RuntimeClock` supplies UTC milliseconds and elapsed monotonic
milliseconds from one runtime-owned Instant origin. Share that same clock
across the runtime; ticks from separate origins are not comparable. Checked
conversion refuses pre-epoch system time, representational overflow or an
Instant before the origin. Wall-clock adjustments do not reset the monotonic
origin; the two values are successive observations, not an atomic clock pair.
`TlsClockSource` cold-retains the injected mail clock and implements the shared
UTC callback. It rejects a failed sample or negative UTC before truncating
milliseconds to whole seconds. It performs no system-clock fallback or cache.

Local TCP pairs and injected outcomes test bounded counts, Pending, both
half-close orders, idempotence, cancellation/drop and sticky failure. Injected
time tests conversion/refusal and recovery of a healthy shared clock source.
These helpers allocate no td-owned per-operation buffers, but do not prove
platform allocation, whole-process bounds, slot ownership or TLS integration.

### 1.3 Implemented TLS record progress

M07d2 supplies `tls_io::TlsIo<T: Transport, B: TlsWireStorage>`, consuming a
handshaking shared session, one exclusive transport and two distinct
caller-reserved 18437-byte ciphertext buffers. TlsWireStorage is sealed to mutable
array references and Box-owned arrays; data access cannot invoke arbitrary
caller code or grow storage. Borrowed buffers remain exclusively borrowed for
the connection lifetime. Owned buffers must be allocated at startup, before
admission; the pump never constructs or replaces them. They let the complete
connection move between workers without self-references or unsafe code.
Each operation progresses at
most one outgoing and one incoming record, with at most one underlying read
and one write. Headers are assembled before bounded bodies; partial records
and short write tails remain in those buffers. Intake tells td-crypto whether
an undrained socket tail remains. Application reads/writes are at most 16 KiB.
Outgoing assembly may make two nonempty in-memory facade drains, for header
then body, in addition to the empty health probes. Incoming assembly yields
after its one header read, so a record needs at least two progress calls.
Partial progress and idle Pending are intentionally indistinguishable here;
M07e must count those calls and the scheduler's polling delay in its latency
and throughput qualification. One record is an upper bound per turn, not a
promise that every ready turn completes a record. The 18437-byte reservation
does not enlarge td-crypto's accepted size: reject bodies >=18432 before
reading them, then let the facade enforce negotiated protection limits.
There are no internal retry loops, growing collections or td-owned hot-path
buffer allocations. The native TLS session still has its separately budgeted
allocations and key/clock locking; this is not a latency or RSS qualification.

Construction fixes whole-connection and handshake deadlines in the injected
clock's monotonic domain. The latter cannot exceed the former. Progress
checks deadlines and shared crypto health before and after work, including
retained socket tails. The handshake deadline applies to the whole call that
publishes success, even if Finished arrives or its final output drains during
that call. Expiry, missing TLS time, provider refusal or transport failure
aborts both handles, clears logical buffers and evidence, and preserves the
first error. TLS errors map to Tls; local deadline/transport errors retain
their mail variant. Aborting discards buffer contents logically, without a
memory-erasure claim. Empty reads/writes remain Pending even after failure.
`into_buffers()` consumes and aborts the connection, then returns both original
buffer owners for safe pool reuse, preserving their storage type and addresses.
It does not expose the transport or session.
Constructor failure returns `TlsIoRefusal` after aborting both handles. Its
fixed `error()` code and consuming `into_buffers()` preserve those same
reservations on phase, clock or deadline refusal. Debug omits buffer contents.
A pool must recover both buffers on either success teardown or constructor
refusal. Dropping an owned connection/refusal instead frees its owned buffers;
there is no implicit pool return. Borrowed storage remains with its original
owner. Runtime leases, return queues and their capacity guarantees remain
M07d3; accepting owned storage alone does not implement admission.

Only an explicit successful `handshake()` permits application bytes. It
requires verified Finished, drained native/output buffers and a completed
underlying flush. `evidence()` returns cached raw cryptographic evidence,
not a fresh policy authorization. This foundation implements Transport, not
TlsTransport: the retained mail wrapper in §1.8 adds generation/count leases,
socket handoff and gateway checks. Complete runtime slots and protocol reset
remain M07d3/M07e. No service entry point uses it yet.

Flush drives record output through the underlying transport and does not
prove peer receipt or handshake completion. Close queues close_notify, drains
output and closes only the underlying write half, preserving incoming data.
A completed close stays idempotent without more clock or I/O work; later
reads still enforce the deadline. Nonempty writes after local TLS closure
fail terminally. EOF requires a complete framing boundary and authenticated
TLS closure. Drop aborts any remaining connection ownership.

A failing post-operation check can occur after bytes entered caller output or
the kernel. Callers discard output on error, and queue effect fences must
precede writes; no error proves that already accepted output was undelivered.
Local public-facade TLS 1.3 fixtures cover tiny duplex pipes, fragmented I/O,
simultaneous full chunks, flush backpressure, close/EOF, invalid counts,
deadline/time refusal, repeated borrowed/owned buffer reuse, owned connection
handoff through an ordinary worker thread, and a TCP loopback exchange.
The portable harness also selects these eleven TLS cases and the five
clock/TCP cases from td-mta's library test executable under isolated musl.
Each must report one passing test; missing or renamed cases refuse artifact
publication. This adds target behavioral coverage, not resource admission.
Shared td-crypto fixtures qualify TLS 1.2 as well. The gateway process
fixtures below cover both versions through the admitted mail transport;
complete SMTP/STARTTLS and service/resource acceptance remain M07d3/M07e.

### 1.4 Handshake capacity reservations

M07d3a supplies `tls_admission::HandshakePool` and its non-clonable
`HandshakePermit`. Construct one service-global pool at startup with the
validated handshake limit (1 through 8). Construction allocates one shared
bitmap owner; reserving and dropping a permit allocate no replacement storage.
Each reservation makes one atomic compare/exchange attempt. Saturation or a
racing change returns Busy without a reservation; the scheduler may retry on
a later turn. `available` is only a momentary observation. No operation spins,
locks a mutex or waits for a worker. The bitmap accounts for capacity only;
it does not publish or synchronize transport-slot payloads.

The permit moves between workers and releases its bit on Drop, including a
returned-error path. Keep it across queued, running and idle handshake steps,
and release it only after successful completion or terminal teardown. It may
outlive the public pool handle: the shared bitmap remains until the final
owner drops. Forgetting a permit leaks capacity but cannot allow excess
reservations. Constructing another pool is a separate capacity domain, never
a way to replace a live service pool or bypass its global limit.

This helper does not reserve wire/session memory, bind a policy generation,
construct a TLS session or enforce a STARTTLS transition. The admitting
factory must couple those owners before sending 220, and release them after
refusal or completion. Pending integration must not treat possession of this
scalar reservation as TLS or gateway authority. The portable runtime exercises
all configured capacities, saturation/reuse, overlapping reservation/release,
worker return and cleanup after refusal or public-pool destruction. These are
ownership/capacity observations, not whole-process allocation measurements.

### 1.5 Immutable gateway client policy

M07d3b1 supplies `gateway_policy::GatewayPolicy`. Cold construction consumes
borrowed validated gateway settings, the protected CA file's contents, an
admitted server identity and the shared crypto clock. It owns the profile
name, current/optional next leaf pins, one through eight canonical peer
prefixes and an opaque server configuration. It admits the explicit private
CA bundle through td-crypto, then selects SMTP, MatchPresentName and mandatory
client-certificate authentication. Invalid material publishes no policy;
there is no public-root fallback. The caller binds these inputs to the
validated listener and charges all material to its certificate generation.

`new_session` creates a raw handshaking session from that configuration.
The runtime must reserve session/handshake resources first. `matches` is a
bounded predicate over a supplied address and leaf digest, using the shared
constant-time digest comparison for both configured pins and SCHEMA.md's
mapped-peer CIDR rules. Neither construction, matching nor session creation
grants mail authority. The admitting transport must obtain verified client
leaf evidence from its completed TLS session, pair it with the actual socket
peer, and recheck the current authorized policy before mutation. Generic
retained ownership is described in §1.6; policy compilation and retained
transport ownership are in §1.7–1.8. Current-generation mutation fencing and
protocol integration remain M07d3c/M11/M13.

`GatewayFingerprint` identifies the canonical client policy for later reload
comparison. It is SHA-256 of the following concatenation; lengths/counts are
unsigned and multibyte integers are big-endian:

| Field | Encoding |
| --- | --- |
| Domain separator | ASCII `td-mta/gateway-policy/v1` followed by one zero byte |
| Profile name | u16 byte length, then the validated ASCII bytes |
| Private CA material | u16 count, then sorted 32-byte SHA-256 digests of each complete decoded certificate DER |
| Accepted leaf pins | u8 count, then the one or two sorted 32-byte pins |
| Peer prefixes | u8 count, then sorted entries of family u8 (4 or 6), prefix bits u8, address 16 bytes |

Sort byte arrays lexicographically; sort prefix entries by family, bits, then
address. An IPv4 address occupies the first four bytes in network order and
has twelve trailing zero bytes; IPv6 uses all sixteen network-order bytes.
Duplicate trust material/pins/prefixes are refused by their admission layers.
PEM whitespace/wrapping, CA order, pin position and equivalent prefix
spelling/order do not change the result. Different complete CA DER counts as
a material change even when a reissued certificate reuses the same key.
Adding/removing even a redundant prefix changes the configured set. The CA
file path and server identity are excluded: moving identical CA material or
renewing the server certificate alone does not revoke client authority.
Listener binding changes are checked separately; this digest does not replace
a generation lease or identify a persistent storage format. Debug omits pins
and fingerprint bytes. Hashing uses only the public td-crypto facade.

Host and portable fixtures cover canonical equivalence, meaningful changes,
pin/CIDR refusals, private material bounds and missing client authentication
against the same server that succeeds with authentication disabled. This is
not a successful authenticated-gateway transport fixture; that integration
and resource qualification remain pending.

### 1.6 Retained generation ownership

M07d3b2a supplies `generations::GenerationSet<T>`, one cold retention domain
for at most two payload generations. Create the service's certificate domain
explicitly with `at_startup` and keep it while any candidate/session retains
its material. There is no Default implementation that could silently replace
a live domain through `mem::take` or a containing service's derived default.
A new set is a separate domain, never a way to bypass an old live set. The
loader must supply a bounded, valid payload and preserve its immutability;
this generic owner does not validate configuration or grant TLS authority.
Resources extracted/cloned from a payload must retain the associated lease.

`reserve` returns a non-clonable GenerationConstruction, capturing its active
base and retaining capacity without borrowing the set. It allocates nothing.
Move that owner through the runtime's control slot before calling `construct`
on the cold worker. Its loader returns Box<T>, so the API does not pass the
whole payload through the control stack; loaders must still bound their own
stack and temporaries. The cold `prepare` convenience combines both steps for
a control worker that already owns the set. Main must not hold a lock across
that loader callback. Dropping an unused reservation returns capacity.

Current, retired, reserved and prepared owners share two slots. Saturation or
a racing reservation returns Busy without spinning or calling a loader.
ID issuance makes a second, single atomic attempt on a process-wide counter;
a concurrent reservation in another domain can also return Busy. Retry either
transient refusal on a later turn, never by spinning. IDs are nonzero u64,
never reset/reused or wrapped; exhaustion refuses before a loader runs.
Returned loader failure releases capacity. IDs are local lookup identities,
not persistent or remote authority.

A non-clonable PreparedGeneration captures its active base. `publish` requires
exclusive coordinator ownership, the original retention domain and that same
active base. Foreign/stale publication returns a fixed refusal plus the whole
candidate and leaves the active generation unchanged. Success returns the
former active lease inside a must-use RetiredGeneration wrapper; it does not
destroy a payload inside publication. The coordinator may be main, and must
move that owner to the control worker before disposal. Refused candidates
also retain their owners for control-worker disposal or recovery. Explicit
Drop still runs on the caller's thread; the helper performs no automatic
handoff. `into_lease` explicitly recovers the optional former active lease. Pending-candidate supersession/deadlines and
complete loader validation remain the runtime's responsibility. The helper
alone neither limits outstanding reload commands nor implements M19.

`current` and lease cloning retain existing backing storage without allocation.
The runtime bounds the number of leases through its slots/jobs. A lease can
outlive the public set, and its last owner destroys the payload before returning
capacity. The payload is separately boxed so its allocation is freed before
the reservation is released; only the small shared ownership header remains
until the enclosing Arc finishes dropping. Charge that transient metadata and
allocator bookkeeping to the same existing ledger. Acquire/release reservation
ordering fences destruction before slot reuse, but does not synchronize
mutable payload access or publish application authorization.

Construction uses cold allocations for the bitmap, loader-supplied boxed
payload and shared owner; std allocation failure and unexpected
loader/destructor panic follow
the existing process-failure contract. This is a retained-payload count, not a
byte or whole-process bound. Debug omits payload contents. Host and portable
cases cover two-generation saturation before construction, stale/foreign
publication without losing owners, returned construction failure, ID exhaustion,
worker construction/retention/drop order and racing construction/release. A
boxed 1 MiB fixture is constructed on a thread with 256 KiB requested stack;
this is not complete control-worker or TLS stack qualification. Sections
1.7–1.8 add TLS tables and retained session/count ownership. Complete runtime
slots, protocol transitions and current-policy mutation fencing remain pending;
this helper enables no service endpoint.

### 1.7 Compiled TLS policy generations and queued session preparation

`tls_policy::TlsPolicies::prepare` consumes a detached GenerationConstruction
and borrows a ResolvedText configuration. The latter proves whole-reader
structural closure plus resolved text inputs, not protected-descriptor trust.
On the cold control worker the compiler admits every certificate profile's
complete derived name set, key/chain consistency and current validity through
td-crypto. It builds at most 18 immutable policies: the configured TLS listeners,
one relay and optional ACME client. HTTP-01 and loopback plaintext fixtures have
no TLS policy. No native configuration or identity handle escapes the table.

The trusted opener receives a redacted MaterialRequest with an explicit kind,
optional profile and optional path. Files-mode identities use configured paths;
ACME chain/key requests name the profile and have no operator path. They load
existing service-managed material, never initiate issuance. Complete preparation
requires every server identity to be present and valid. Explicit prepare_clients
supplies the earlier startup/recovery stage: it compiles only relay and optional
ACME policies, opening only their explicit CA inputs. It does not open or admit
server identities or gateway trust. Missing/expired local server material cannot
block these outbound roles. Both modes still require a structurally closed,
text-resolved configuration and refuse invalid/unreadable explicit client trust
without fallback. This is not an automatic fallback on complete-table failure.
PolicyCoverage reports ClientsOnly or Complete as compilation metadata; it is
not a service-health or runtime-publication claim. Client-only listener lookup
returns NotFound, and a different generation's saved listener ID is refused.

Use the same GenerationSet for both modes. Publish the client-only generation,
perform issuance/recovery on the outbound slot, then prepare and publish a
complete replacement after usable server material exists. Retained outbound
sessions can finish on the old generation; both tables share the same two-slot
limit. Failed complete preparation leaves clients selected. A client-only table
may also replace a complete table when runtime expiry policy disables inbound
service; no stale gateway binding remains in the selected table. M18/M19 must
still wire startup/expiry health, issuance, retry and atomic runtime publication
before service activation. These constructors perform no ACME network operation.

Explicit CA requests retain their configured role. Missing relay/ACME overrides
select pinned public roots; malformed or unreadable explicit input never falls
back. Complete preparation checks every staged gateway CA, even without a
listener, and reads each used gateway bundle once for all of its listeners in
this generation. Client-only preparation never opens gateway material.

Readers must implement truthful Read/EOF and bounded, deadlined I/O with M05's
protected-file and secret/public inode checks. The compiler itself performs no
filesystem or network access. It reads through EOF or the ceiling plus one byte,
refuses oversized and impossible counts, and permits at most 32
Interrupted results across the whole read, matching config material readers.
Positive progress never resets that allowance; at most ceiling + 34 calls
cover one-byte progress, overflow/EOF and interrupted attempts. Chain/key
caps are 64/16 KiB; CA caps are 128 KiB. Raw windows clear on drop, including
errors/unwinds, without a secure-erasure promise. An opener panic follows fatal
service policy. Failure destroys the candidate and returns its generation slot;
active publication is unchanged.

Direct SMTP uses its profile with DefaultIdentity, HTTPS uses RequiredName
with its primary and selected MTA-STS profiles, and gateways require their
private client trust plus separately retained pin/prefix policy. Gateway
identities narrow to that listener's server_name before MatchPresentName
selection. Complete identities first validate all graph-derived names.
HTTPS then narrows
the selected profiles to exactly the JMAP origin and their MTA-STS names, sharing
existing key/chain ownership. This avoids treating SMTP names as HTTPS routes
or introducing name collisions across shared profiles. HTTP authority routing
still separately admits only the configured JMAP/MTA-STS authorities.
Relay and ACME
use their configured host, port, trust and SMTP/HTTP protocol. Destination
metadata and PolicyRole are configuration, never authenticated peer evidence.

Listener lookup uses the configured name, not a saved row index. Returned
TlsPolicyId contains the retained generation ID and checked table index;
resolution refuses a different generation or invalid index. A reload can
reorder rows. `gateway_unchanged` compares the selected old policy with a row
in the supplied current table using its canonical gateway fingerprint and all
listener stanza fields (name, kind, numeric bind, server/certificate/gateway
names, session and per-peer limits). It ignores source locations and row order.
Server material renewal alone does not revoke client policy; semantic trust/pin/
prefix changes, including broadening, or binding changes do. The caller must
supply the actual active generation and serialize this predicate with mutation
publication. Passing a retained old table as current proves nothing. This
helper supplies neither authenticated gateway evidence nor that runtime fence.

`reserve_session` validates the policy ID and reserves the shared HandshakePool
before returning non-clonable SessionPreparation. It owns a generation lease,
permit and both caller-reserved wire buffers, including while queued. It makes
no allocations and performs no native TLS construction or I/O. A fixed TLS
worker consumes `construct` to create the opaque native handshaking session;
SessionReservation keeps all owners together. Status remains raw crypto status,
not mail authorization. Both wrappers are must-use and cannot expose/clone raw
configuration/session handles. Native destruction precedes generation and permit
release. Preparation or native teardown returns both original arrays explicitly;
ordinary Drop frees Box-owned arrays without returning them to runtime pools.
Refusal returns a fixed error and both arrays, releasing permit and generation.

The runtime separately owns the complete session slot, native byte allowance,
queue/completion credit and bounded generation lifetime. Construction is not
whole-resource qualification. Socket handoff and completed-handshake permit
release are described below; current-policy mutation fencing and STARTTLS
protocol transitions remain later increments. No listener or service entry
point uses these owners yet; M07e still gates native allocation/stack/RSS and
aggregate admission.

### 1.8 Retained TCP/TLS connection

SessionReservation::handoff consumes an exclusively owned TcpTransport, the
injected runtime Clock and fixed whole-connection/handshake deadlines. It
captures the actual socket peer; no caller-supplied IP can replace it. Use the
same clock source/origin as policy preparation. Reject nonempty plaintext tails
before TLS progress, abort both handles on refusal, and return the two original
wire arrays. Constructor deadline/clock refusal has the same recovery path.
The runtime must reserve before SMTP 220, flush that reply completely before
handoff and bound the queued job lifetime. The adapter cannot prove protocol
framing or flush from an empty slice. ServerStartTls below implements the
inbound reply boundary; full state reset and outbound upgrades remain
protocol integration work.

TlsConnection implements TlsTransport and Transport, with private native state,
a retained generation, handshake permit and two preallocated wire reservations.
It neither allocates another wire buffer nor exposes a socket/native handle.
Native teardown occurs before releasing the generation. Pending handshakes
retain their permit, including progress driven by read/write/flush. Explicit
handshake completion requires verified Finished and a drained local flight,
then maps evidence according to the retained role: ordinary inbound is None,
relay/ACME require VerifiedServerName, and gateway requires VerifiedClientLeaf
plus the configured current/next leaf pin and CIDR match on this actual peer.
Any role/evidence mismatch refuses. Gateway matching never trusts a presented
unverified digest. Local process fixtures now cover positive gateway mTLS
for current/next pins and refusal of a verified leaf with a wrong pin or actual
socket peer, under both TLS 1.2 and TLS 1.3. The remote client lives only in
td-crypto's private test binary; mail tests use the production public facade.
The fixture exchanges EHLO/reply bytes after TLS, without claiming a complete
SMTP parser or STARTTLS state machine.

Only that successful mapping publishes cached TlsInfo and releases the
handshake permit. Before it, nonempty read/write may progress bounded TLS work
but return Pending without touching application bytes. Empty slices return
Pending even after failure. Every progressing I/O call uses the record pump's
clock/health checks and fixed deadline. handshake's deadline may tighten the
original handshake cap, never extend it; the shortened cap persists across
calls. Established sessions retain the original whole deadline and may outlive
the handshake deadline. Cached info is neither a fresh health sample nor
current-generation authority. Failures clear it, abort native/socket state,
release a pending permit and preserve the first error. Orderly close retains
the read half and follows the pump's idempotent close semantics.

Call check_gateway_policy only for gateway connections: on another role it
returns Invalid and aborts the connection. It compares retained canonical
policy and the complete listener binding with the caller-supplied current
table. Removal or any semantic change, including broadening, aborts and
clears cached authority. This method requires the trusted runtime to supply
its actual current generation and serialize check/publication/mutation;
passing an old retained table does not prove freshness. It is not a
standalone mutation fence. Current-generation ownership and durable mutation
integration remain M11/M13.

into_buffers aborts and destroys the native pump before releasing its generation
and any remaining permit, then returns both original arrays for explicit pool
reuse. Ordinary Drop aborts through the pump, releases owners and frees owned
arrays. Complete runtime session/byte slots, return queues and aggregate
qualification remain required before service activation. No listener is enabled.

### 1.9 SMTP control framing

`smtp_wire::LineReader` borrows one 512-byte startup reservation and
recognizes strict CRLF across arbitrary chunks. It accepts ASCII printable
control-line bytes and HT; DATA has separate framing. Its limit includes
CRLF. `feed` stops at the first complete line and reports the exact consumed
prefix. The caller retains the entire tail, including pipelined plaintext or
TLS-looking bytes; the reader never silently discards it. A complete line
remains borrowed until explicit `advance`. Invalid framing or capacity is a
terminal error. Advancing a complete line clears its storage; constructing or
dropping a reader does not. Final/failed line bytes may remain in the caller
reservation but cannot be observed through a fresh reader. No secure erasure
is promised. EOF before a complete line is a protocol failure owned by
the transport driver; this byte parser does not read sockets or time out.

`ReplyReader` uses that same reservation for one reply, checking each
three-digit SMTP code, separator and text. Every continuation line has the
same code. A bare final code is accepted. The 16 KiB aggregate ceiling counts
all wire bytes including every CRLF. It emits each complete line separately;
callers advance continuation lines and retain recognized extension facts
only after the final successful reply. A complete reply cannot advance into
the next reply; construct a fresh reader over the reused reservation.
Syntax/capacity failures clear observable reply evidence and remain terminal.
V1 rejects non-ASCII reply text as a protocol failure, even if its leading
code appears valid. Never synthesize smtpReply or permanent refusal from that
prefix. QUEUE.md's protocol-failure/retry/uncertainty rules apply; this can
retry a nonconforming relay's non-ASCII refusal until expiry. UTF-8 message
content under 8BITMIME does not change control-reply syntax. M17 must bound
normalized persisted reply text separately to its 4096-byte storage ceiling;
a 16 KiB wire-work limit is neither that storage limit nor an allocation.

`ehlo_extension` parses extension keyword/parameter syntax only for a 250
reply line after the first greeting. Callers compare keywords without ASCII
case; a greeting containing STARTTLS is never an advertisement. An Invalid
extension result affects that line only: skip it as unadvertised and continue
the correctly framed reply. This includes legacy AUTH= syntax and trailing
spaces. A later standard AUTH line still counts; absent required capabilities
refuse the operation. Recognized keywords still require their own parameter
semantics (STARTTLS has none). Neither syntax helper authorizes TLS,
implements a command transaction, or proves a
reply has been flushed. STARTTLS drivers still own reservation-before-220,
complete output flush, exact-tail refusal, handshake and parser/EHLO/auth
reset. M10/M17 own the remaining SMTP protocol and extension semantics.
These readers borrow existing SMTP/outbound scratch; they allocate nothing
and add no resource reservation or service listener. The 512-byte reader is
for base control lines; M10 must add the larger MAIL command ceilings required
by advertised SIZE/8BITMIME before using it for the complete receiver.

### 1.10 Inbound STARTTLS reply ownership

`tls_policy::ServerStartTls` consumes a prepared native session and the
exclusive TcpTransport before sending any 220 bytes. Construction requires a
complete LineReader containing bare, case-insensitive STARTTLS, an empty
unread tail, a DirectSmtp/GatewaySmtp policy and valid fixed whole/handshake
deadlines. Refusal aborts the socket and destroys native state before returning
the original two wire buffers and releasing the handshake/generation owners.
No plaintext response is emitted on constructor refusal. The SMTP driver
validates command sequencing and emits any syntax/temporary refusal before
entering this owner; this primitive does not replace the command dispatcher.

`advance(self)` performs one bounded write or flush and returns Pending with
ownership, or consumes the owner into TlsConnection only after the entire
220 reply and flush complete. Short writes and Pending preserve the offset;
invalid counts, clock/transport errors or deadline expiry abort and return
SessionRefusal with both buffers. Time is checked before and after each
operation; crossing the deadline can occur after bytes were already sent.
There is no plaintext fallback and no renewed deadline. The returned TLS
connection still requires a successful verified handshake before application
I/O. Explicit cancellation returns the reserved buffers; Drop closes/releases
owned resources without inventing a pool return queue.

The trusted driver supplies the reader and exact tail from this socket,
drains any other plaintext output, retains no other plaintext input, and
supplies a still-authorized retained generation. This owner does not prove
that caller context or implement an actual-current mutation fence. Full
SMTP parser, EHLO/extension, authentication and transaction-state reset after
successful TLS remains M10/M13 integration. The outbound boundary follows.
Tests cover short writes/flush backpressure, clock/deadline/refusal cleanup,
real direct SMTP handoff and private gateway client-certificate peers under
TLS 1.2/1.3. These fixtures start at the STARTTLS command boundary; they do not
implement the initial SMTP greeting/EHLO or the complete mail protocol.
Existing session/wire reservations are retained; no worker, allocation or
resource allowance is added. M07e still gates whole native/session resource
qualification before service activation.

### 1.11 Outbound STARTTLS reply ownership

`smtp_wire::EhloReader` borrows a 512-byte reply reservation and retains only
whether a non-greeting 250 extension advertises parameterless STARTTLS.
It uses the same strict framing, aggregate ceiling and local malformed
extension skipping as ReplyReader. Consuming a complete successful reply with
an empty exact tail yields an opaque, non-copyable StartTlsOffer. Partial,
failed, tailed or unadvertised replies cannot produce one. The offer is syntax
evidence; the trusted driver binds it to the actual socket and EHLO exchange.

`tls_policy::ClientStartTls` consumes that offer, the exclusive TcpTransport
and a prepared session whose retained relay policy requires STARTTLS. Implicit
relay and server policies refuse before any output. UpgradeScratch borrows two
distinct existing reservations, using 512 bytes of each for socket input and
reply assembly; insufficient space refuses. Construction and each consuming
advance check fixed whole/handshake deadlines. Each advance performs at most
one socket operation and parses at most one reply line. It writes and flushes
STARTTLS, then requires a complete, bounded 220 reply with consistent multiline
codes and no buffered tail before consuming ownership into TlsConnection.
Only subsequent TLS handshake progress may send ClientHello. No credentials
are emitted and no plaintext fallback occurs.
The lower-level SessionReservation::handoff remains available to protocol
adapters. Its callers must enforce the configured STARTTLS exchange themselves;
a retained policy role alone does not prove command sequencing.

Reply framing/capacity errors, non-220 replies, EOF, clock or transport errors
and expiry abort the socket/native session and return SessionRefusal with the
original wire buffers. Explicit cancellation does the same cleanup without an
error. The caller regains its borrowed scratch when the owner ends; neither
scratch nor consumed EHLO state has a secure-erasure guarantee. An unread tail
means bytes already read into these buffers; later socket bytes are handled by
the TLS record parser, never by a resumed plaintext parser.

The trusted driver drains previous output and retains no other plaintext
input or capabilities. The offer does not prove socket identity, command
sequencing or current-generation authority. Tests supply the EHLO capability
boundary, then exercise real local command/reply exchange and both upgrade
owners through verified TLS. Complete greeting/EHLO dispatch, fresh post-TLS
EHLO, AUTH and transaction reset remain M17. M07e must account for these
borrowed reservations within the complete outbound session ledger before
activation; this API grants no new resource allowance.

## 2. Read views and change history

ReadView pins account/epoch, checkpoint generation and sequence, active segment,
exact committed byte prefix and committed sequence, and retained history floor.
It never observes later commits. Pins include history needed by next_change;
if unavailable, acquisition/iteration returns HistoryLost, never a partial
successful change page. Deadline/pool expiry invalidates further operations.
history_floor is the last sequence whose frame need not remain: all frames
strictly after it through the committed endpoint are retained. Thus a since
state equal to the floor is valid. GC/checkpoint retention obeys those pins.
Detached Record values borrow only
caller key/value storage; holding them does not keep a view or blob alive.

get and next use shared row/key semantic validation and verify checksums before
exposing bytes. Buffer shortage returns Capacity, not a truncated record.
next visits strictly increasing encoded keys in the specified table, with
None meaning exhaustion; after=None starts at the first key. Validate a supplied
after key for that table. next_change scans complete retained frames after its
cursor through the pinned committed sequence. It returns matching CHANGEs in
(sequence, operation ordinal) order, skipping PUT bytes with bounded I/O.
Read action from operation-header byte 1 (FORMAT.md); the empty CHANGE value
does not remove its created/updated/destroyed header action.
Operation ordinal is zero-based across all frame operations; u32::MAX denotes
the end of its sequence. A cursor below history_floor gives HistoryLost, above
the pinned end gives Invalid. Identity is not a journal-backed change kind.
No frame-sized allocation or arbitrary caller cursor may cross the pinned end.
Each call returns Record for one matching CHANGE, Advanced at the next
complete frame boundary (even with no matching changes), or Complete only
at the pinned endpoint. Stop scanning at that first record/boundary; a call
reads no more than one bounded frame after locating its cursor. The view
retains its streaming cursor so repeated calls do not rescan earlier frames.
After Advanced, the caller resumes at (through, u32::MAX). The coordinator
can publish a bounded advancing empty page or a prior fitting boundary when
the next frame exceeds its remaining budget; it never skips unseen changes.
A below-floor or invalid cursor fails before scanning, and deadline expiry
is an error, not Complete. Locating an initial cursor also has a work budget.

`frame_changes::Collector` is a supplied-byte building block for that future
cursor. It wraps the incremental frame verifier, accepts one exact operation
per push and copies each CHANGE's type/ID/action/ordinal into caller Cell slots.
It validates PUT/DELETE bodies but retains none of their bytes. A slot shortage
returns OutputFull and permanently fails collection, as do malformed operations
or provider errors. No partial list or success boundary is exposed. Consuming
finish checks the complete frame and returns CompleteChanges with its Summary
and read-only access to populated slots. Consuming into_cells returns the
original full mutable scratch capacity for the next frame, ending that result's
slot borrow; retained unused slots never appear in records. Records have the
frame sequence and original ordinals; duplicates/actions are not coalesced or
filtered. Row-only frames can complete with zero slots. Results do not borrow
the operation input and retain no file or view pin. RESOURCES.md reserves
distinct change slots because get/next may use their result buffers while a
completed frame is being drained.
This helper performs no I/O, cursor/floor/endpoint check, selected-journal
binding, JMAP coalescing or next_change activation.

`change_cursor::Cursor` implements the supplied-frame policy for one fixed
object kind and captured ViewIdentity. Construct it with an initial after
cursor; Identity, a future sequence or an inverted floor/end range is Invalid.
A cursor below the floor, or partway through the floor's unretained frame, is
HistoryLost. The floor boundary `(floor, u32::MAX)` remains valid. The caller
owns actual pins and verifies deadlines, selected files and final-view validity
before using any returned step as serving data.

`poll` requires the identical captured identity and the exact last returned
cursor. View changes are Conflict; caller cursor jumps or reuse of an older
cursor after progress are Invalid. With no supplied frame, NeedFrame names
the next exact sequence;
locating/reading it has an external work budget. Supply a CompleteChanges from
a checked frame. The initial finite ordinal must name an operation in that
frame; it need not be a CHANGE. Frames out of order or replacement of the
currently drained frame's Summary are Corrupt. All poll errors are terminal.

Drain matching records in original ordinal order, including duplicates/actions.
A saved slot index never revisits examined changes. One call returns the next
record or Advanced at that same frame's end; no matching records still produces
Advanced. Resume at `(through, u32::MAX)` after Advanced; the next call can
request the next frame. Complete occurs only after the pinned endpoint boundary,
including an empty range. This helper reads no files, coalesces no JMAP events
and supplies no live pin, selected-history validation or protocol activation.

`store_fs::ChangeRoute` selects the source for a NeedFrame sequence using
selected manifest metadata and captured ViewIdentity. It validates retained
coverage through the checkpoint, then returns a history descriptor index or
Active only within `(history_floor, committed_sequence]`. Changed identity is
Conflict; missing history is HistoryLost and future sequences are Invalid.
This immutable lookup performs no I/O and does not retire on a caller error.
The driver still validates/opens files, locates the frame under a work budget
and holds real pins; a source choice is not serving authorization.

BlobReader is an already authorized, opened, immutable file with a live owner
pin held until it closes. Store::open_blob requires Access and borrows the
view, retaining that ownership until the reader drops. It checks live message/
submission references or an authorized unexpired upload lease before opening
the root blob; a known BlobId alone grants nothing. Opening private paths
belongs to M05; protocol handlers do not open files directly. read_at uses
checked offsets,
returns at most output.len() bytes, zero only at EOF or for empty output,
and refuses offsets beyond len. A positive short read is legal. The caller
loops within its work budget. Missing/truncated committed bodies are Corrupt.
Part locators additionally require WIRE.md's authorization/descriptor checks.

## 3. Reserve, publish, commit

Store::reserve runs before accepting a body or mutation responsibility.
Access carries the account, trusted principal and configuration generation.
It is constructed by the coordinator after authentication/admission, never by
deserializing a client field. Recheck principal permissions/device revocation
and the selected configuration at each access and at commit. SMTP, queue and
maintenance principals have only their named operation rights. A stale
configuration generation causes reauthorization against the selected snapshot;
the coordinator can refresh Access without releasing the reservation, its
body quota or pins. Generation mismatch alone is not device revocation. An
actually revoked device cannot commit a device-initiated mutation.

Queue outcome recording belongs to the durable attempt/fence that authorized
the transmission. A later route credential/identity configuration change may
stop new dispatch, but cannot revoke the queue principal's right to record
that already-authorized attempt's final result using its held reservation.
Refresh Access for that narrow recording operation; do not rerun sender
authorization as though it were a new submission. New dispatch uses the new
configuration. This also applies to recovery of the prior attempt.

The account/size/deadline lease has a ReservationId containing coordinator
instance ID, slot and checked generation; a different instance or expired/
reused slot is rejected even if it has the same Rust type.
Include every PUT, DELETE and retained CHANGE in frame bytes/operation count.
Capacity is global across reservations and survives checkpoint transfer under
STORAGE.md's barrier. No unaccounted frame/operation overrun may reach disk.
DiskBudget separately reserves logical raw-blob bytes/files, metadata
bytes/files and logical upload/queue quota increases (including a new pin on
an existing body). Upload/queue categories may overlap raw-blob charges and are not
added to raw use a second time. When size is unknown reserve the admitted maximum.
ADMISSION.md fixes logical quota ceilings and checkpoint output bounds; no blob creation
may bypass this reservation. Exceeding a logical quota returns Quota; temporary
pool/storage pressure returns Capacity/Busy. Dropping or expiring releases
unused capacity once; already-written orphan bytes remain charged until cleanup.
DiskBudget covers store operations. Request retention, sort/cache runs, logs
and cold-state files have separate typed logical leases. No lease reserves
physical blocks or guarantees I/O success. All write/sync failures propagate.

Store::begin_blob borrows an active reservation exclusively and charges its
raw-body quota before creating the private file. BlobWriter writes whole
chunks or fails; after error the entire writer is poisoned and must be
dropped, never retried with the same chunk. publish
consumes it and returns only after file and publication-directory sync.
PublishedBlob has read-only accessors for reservation/account/id/length/digest;
only core persistence constructs it, and it is neither Clone nor Copy. It is
bound to the same still-active reservation that pins its pending blob against
GC. It is not an authorization credential. M05 owns root paths, publication
checks and orphan cleanup. A publication error may leave an orphan but cannot
create metadata. Dropping a writer ends its borrow, not the reservation.

M02c3a refines transaction handoff to TransactionInput: one immutable encoded
byte slice plus operation count, using all FORMAT.md PUT/DELETE/CHANGE
operation headers and key/value encodings without a journal frame header.
There is no separately allocated CHANGE slice. Validate every input byte,
tag, count, length and semantic key/row before append; TransactionInput
is not a proof of validity. Owned StagedOperation offsets (bounded to 32 bytes
per slot, including parsed operation kind/type/action) index the input arena.
Each Row/Key is decoded only while used.
Do not store an array of borrowed Mutations beside their mutable backing arena:
that prevents safe buffer reuse across requests. The single-record Mutation
enum remains an encoder input, not a pooled self-referencing object graph.
M05 supplies the operation encoder/parser; this increment only fixes handoff
and checked startup layout. Frame header/footer bytes must fit in addition
to the supplied operation bytes (which already include CHANGE). Input and
output buffers are distinct. Both arenas reserve `frame_bytes`, but valid
input is at most `frame_bytes - FRAME_HEADER_BYTES - FRAME_FOOTER_BYTES`;
reject a larger operation stream before encoding or append. Unused input
arena capacity does not increase the permitted journal frame size.
The extra input arena is deliberate: protocol-owned canonical input stays
immutable while the store builds and validates its own complete frame. This
keeps the commit interface independent of store-private mutable buffer leases
and gives inspection/fault adapters the same handoff. V1 pays one bounded copy
instead of requiring vectored append or exposing a writer arena to callers.
M05 uses the checked StagedOperation slot representation, or amends its size
contract before substituting a different internal descriptor.

Ports express the thread handoff: transports, digest state, reservations and
blob handles are Send; shared stores, clocks, crypto and read views are Sync.
This does not permit concurrent mutation or erase borrowed lifetimes. Startup
owners outlive scoped workers and their leased handles; queues still carry
slot IDs, never lifetime casts. TLS factories are worker-owned Send values.

Store::commit runs only on the serialized writer worker. Recheck reservation
ownership/deadline, expected account sequence, authorization supplied by the
coordinator through Access, record semantics/references, published-blob
reservation/account/digest/length and matching blob PUTs, and exact encoded size/count before any append.
Every newly referenced blob must be either live in the current view or in
this call's checked published handoff. A handoff alone does not grant access
to an existing blob in another account. Derive/validate CHANGEs from actual
object effects; do not trust protocol callers to omit or forge them. Identity
CHANGE is reserved by the byte registry but rejected in v1 mail transactions.
Repeated keys follow FORMAT.md ordering; changes describe the frame's final
object effect. Never allow a partially valid transaction into the journal.

Effects include derived properties even when their own table row is unchanged:
Email create/destroy changes Thread.emailIds and all affected Mailbox counts;
membership changes update that Email and affected Mailboxes; $seen/$draft
changes update that Email and affected unread counts. V1 chooses RFC 8621
§2's simple mailbox-local unreadThreads definition: count distinct threads
with an email in this mailbox having neither $seen nor $draft. totalThreads
is distinct threads with any member in this mailbox. Compare before/final
counts and emit a Mailbox CHANGE wherever any count or stored property changed.
Emit Thread-created/destroyed when its first/last email is added/removed,
otherwise Thread-updated on emailIds changes. Account for the complete derived
fan-out in the reservation before accepting the transaction. Never omit a
CHANGE to fit a frame. Mailbox/changes.updatedProperties is always null in v1,
because the journal does not track which individual properties changed.

expected is the sequence used to prepare the mutation. A different current
sequence yields Rejected(Conflict) without appending. The coordinator may
replan within its bounded work budget while retaining the same reservation,
including streamed/published body quota and GC pins; ifInState still compares
the client's token against the newly selected view before mutation. A commit returns Ok
only after complete frame sync and durable visibility publication. commit
borrows the reservation mutably. Rejected(Conflict) leaves it active; other
pre-append refusals leave it active unless its deadline/authorization/identity
is invalid. Ok consumes its reserved capacity into committed usage, and
Indeterminate poisons it until recovery. is_active becomes false in either
case; it can never be reused or double-released. An invalid/expired lease
returns unused capacity once and transfers written orphan charges to cleanup.

CommitFailure deliberately separates:

- Rejected: this attempt appended no frame; ordinary object/method error is
  safe if this method has no earlier commits. The active reservation and its
  pending-blob pins survive Conflict; dropping it makes unreferenced blobs
  eligible for orphan cleanup.
- Indeterminate: a partial or complete recoverable frame may have been written.
  Stop all writes and transmitting workers; recover before accepting another
  mutation. Do not fabricate an ordinary failed Set result claiming no effect,
  retry the append or roll back prior successful methods. If the HTTP response
  cannot accurately report the outcome, close the request; reconciliation
  uses committed IDs/history after recovery. Inbound SMTP closes without final
  acceptance, so a sender can retry and a duplicate is possible.

An error after frame durability but before local notification is still an
indeterminate caller outcome. Cancellation/deadline cannot revoke a committed
frame. The contract requires injected failure tests around every boundary;
the enum itself does not implement them.

## 4. Opaque state strings

src/sync.rs encodes into caller storage without allocation. All hex is lower
case, fixed width. Decoders consume exactly the specified length. State strings
are synchronization values, never authentication tokens.

| Kind | Exact encoding |
| --- | --- |
| DataState, 85 ASCII bytes | d1_ + two hex digits of ObjectType tag + 32 account hex + 32 epoch hex + 16 sequence hex (most significant byte first) |
| IdentityState, 131 ASCII bytes | c1_ + 32 account hex + 32 epoch hex + 64 identity snapshot SHA-256 hex |

DataState admits Mailbox=01, Thread=02, Email=03, EmailSubmission=05; not
Identity=04. Its sequence is the account-wide committed sequence of the view.
Thus it changes for every transaction, including unrelated data changes.
This deliberately trades RFC 8620 §5.1's SHOULD for stable unchanged-type
state for a small restart-safe state representation without per-type durable
counters. Empty /changes pages after unrelated changes are valid; a type's
real change can never leave its state unchanged. Restore changes the epoch.

For ifInState, any supplied string not equal to the current scoped token
gives stateMismatch, including an old epoch/type, future sequence, malformed
opaque spelling or a string from another account. A non-string argument is
invalidArguments. For /changes the corresponding unrecognized/out-of-range
token gives cannotCalculateChanges. Valid history range is floor <= sequence
<= current sequence with the same account/epoch/type. Codec range checking
does not acquire a view or prove retained history; the adapter must pin it.

For /changes select one target view, scan only (since, target], and coalesce
each ID by final effect: create then destroy disappears; create then update
is created; update then destroy is destroyed; repeated updates occur once.
No deliberate delete/recreate reuse is allowed. Return IDs disjoint across
created/updated/destroyed and ascending raw ID within each array. Restrict
maxChanges across their combined size. Page only at complete frame boundaries;
choose the latest scanned boundary that fits all effects, pinning that page's
old/new state. If no advancing boundary fits before the work budget expires,
return cannotCalculateChanges. Do not split a transaction or omit excess IDs.
hasMoreChanges means more frames remain through the pinned target; the next
request may see a newer target. An unchanged request at current returns empty
arrays, equal states and false. Retention loss forces explicit resynchronization.

V1 queries return canCalculateChanges=false; /queryChanges returns the standard
cannotCalculateChanges for supported well-formed queries. They do not rerun a
query and pretend its changed ordering is a delta. POLICY.md specifies the
queryState encoding and bounded sort/search semantics; CASES.md names the
wire fixtures. The queryState codec is still owned by M14/M16.

Identity data comes from one atomically selected validated configuration
snapshot, outside the mail journal. Digest preimage is ASCII td-mta-identities-v1
followed by a NUL byte, u32-LE count, then identities sorted by raw ID. Each
identity is raw 16-byte ID, name, email, replyTo, bcc, textSignature,
htmlSignature, mayDelete in that order. Strings are u32-LE UTF-8 byte length
plus bytes, booleans one byte 0/1. Nullable arrays use byte 0 for null or byte
1 + u32-LE count + ordered addresses; each address is nullable name (byte
0, or byte 1 + string) then email string. mayDelete is false in v1. Defaults
are materialized before hashing. Hash every visible property, including
signatures; exclude credentials and unrelated settings. Configuration loader
bounds all counts/text and refuses overflow before digesting/publication.
Unchanged visible identities produce the same digest. Identity/changes with
the exact current token returns an empty change result; any other string
returns cannotCalculateChanges. Identity/set is read-only with normal
preconditions/notFound and forbidden errors. Restore still changes its epoch.
`config::identity` implements the canonical preimage encoder. Its borrowed
`Identity`/`Address` values preserve visible strings and ordered arrays exactly;
the caller materializes defaults and supplies identities in strictly ascending
raw-ID order. Duplicate or descending IDs fail. V1 `mayDelete` is encoded as
false, with no caller override. The codec does not validate mailbox syntax,
authorize From addresses, bind an account, load files or publish a state token.
Empty snapshots are encodable for the empty digest oracle; service-required
identity declarations belong to the full configuration schema.

Representation ceilings are 64 identities, 16 addresses per replyTo/bcc array,
4096 UTF-8 bytes per name, 254 bytes per email, and 16384 bytes per signature.
The configuration line parser still caps decoded strings at 4096 bytes;
SCHEMA.md specifies signature-file references for all configured signatures,
including multiline values, with protected-file validation in M04b3/M05. The complete loader is
still unimplemented; this encoder provides no way around the input grammar.
The total encoded preimage is at most 192 KiB, including prefixes/counts. These
are simultaneous upper bounds, not a guarantee that every maximum combination
fits. Text borrows the snapshot's existing non-routing text region; the encoder
reserves no additional arena. The future loader must also fit identity
metadata and every other non-routing value inside RESOURCES.md's snapshot.

`encoded_len` validates all representation bounds and ordering. `write_preimage`
performs that complete check before any sink call, then streams the encoding
without heap allocation or a full preimage copy. Length prefixes use UTF-8 byte
counts. Null arrays and empty arrays, and null address names and empty names,
remain distinct. A sink failure ends emission immediately; its partial digest
or output must be discarded, with no state publication. The trusted sink owns
its own allocation/failure behavior. Sink calls never contain empty slices.
Default error Display/Debug and value Debug do not expose identity text; typed
sink errors remain accessible through the enum and error source, whose
explicit inspection is the trusted caller's responsibility. Validation reports
the first refusal in encoding traversal order; no cross-field diagnostic
precedence is promised. M07 owns hashing with the
real provider, and M15 owns state-token publication from a pinned snapshot.

Stable representation codes are `identity_preimage_count`,
`identity_preimage_address_count`, `identity_preimage_text_limit`,
`identity_preimage_size_limit` and `identity_preimage_order`. They identify
codec refusals; the schema/CLI must supply configuration source locations.

Identity digest oracles (literal preimages; M07 verifies with the real
provider, M04 verifies the canonical encoder):

- Empty snapshot preimage hex:
  `74642d6d74612d6964656e7469746965732d76310000000000`.
  SHA-256: `2804d97c45f30188d110f0e527fde929ee29404879c87d717b5085a4c8ca4830`.
- One identity: ID is sixteen 0x11 bytes, name Me, email me@example.org,
  replyTo null, bcc empty array, empty signatures, mayDelete false. Preimage:
  `74642d6d74612d6964656e7469746965732d7631000100000011111111111111111111111111111111020000004d650e0000006d65406578616d706c652e6f7267000100000000000000000000000000`.
  SHA-256: `5b5bb9f27cd7a82bbc8e09db63825ad730612515f87d873845e09808640747ce`.

Session state is a separate capability/account/endpoint/configuration
fingerprint (M13); it must not expose volatile queue counters or secrets.

## 5. Error translation

Authorization precedes existence lookup. Core Invalid/Corrupt must not turn
internal invariant failure into a client invalidArguments error. Validate
client syntax in the protocol layer, then map only documented user mistakes.
Unknown well-formed IDs have notFound semantics as specified by WIRE.md.

| Condition | Protocol behavior |
| --- | --- |
| No admission/read slot before HTTP headers | Retryable HTTP 503 with bounded Retry-After |
| Temporary Busy/Capacity/Deadline before this JMAP method has any effects | serverUnavailable; earlier method successes remain in the response |
| Deterministic per-request output/work ceiling, before this method's effects | serverFail with the exhausted limit; an identical retry is not promised to fit |
| Request-level maxSizeRequest/maxCallsInRequest/maxConcurrentRequests exceeded | HTTP problem details of type urn:ietf:params:jmap:error:limit with the limit property, before processing any method |
| Method object/count/size or logical quota limit | requestTooLarge or method-specific tooLarge/overQuota as the applicable RFC requires; never reuse requestTooLarge for arbitrary output size |
| Client ifInState mismatch | stateMismatch before object mutations |
| HistoryLost/unrecognized changes state | cannotCalculateChanges |
| Missing authorized object/blob | notFound / HTTP 404 |
| Forbidden per-object operation | forbidden or the specified method-specific SetError |
| Corrupt/WriterStopped/Indeterminate | Fail service health, stop mutations; never acknowledge an unproved commit |

Method-level errors other than serverPartialFail require no externally
visible change by that method (RFC 8620 §3.6.2). If earlier objects in a Set
method committed, return normal per-object results where the remaining
refusals have truthful defined SetError semantics. Predictable resource
exhaustion must be refused before mutation. An unavoidable later failure
without a suitable standard SetError uses serverPartialFail and requires
resync; never replace known effects with method-level serverUnavailable or
serverFail. RFC 8620 §5.3 does not enumerate those generic method failures
as SetErrors. ADMISSION.md specifies the exact retained-response behavior.
Reserve response capacity before mutation. An indeterminate journal outcome
follows section 3, never a guessed result.

SMTP rejects unavailable admission with temporary status before DATA and
returns final 250 only after proven durable commit. No local error maps to
acceptance. After HTTP headers are sent, a read failure closes the incomplete
response; never emit a second HTTP status or valid-looking truncated JSON.
ADMISSION.md supplies response retention so later method failures cannot erase
earlier method successes. Tests in M02c2 cover token bytes/bounds/scoping;
future adapter and protocol suites must prove these operational mappings.

Sources: [JMAP Core, RFC 8620 §5](https://www.rfc-editor.org/rfc/rfc8620.html#section-5)
and [JMAP Mail, RFC 8621](https://www.rfc-editor.org/rfc/rfc8621.html).
