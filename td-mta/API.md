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

### 1.12 MIME base64 octets

mime_base64::Decoder implements POLICY.md's stable base64 transfer octets.
poll borrows encoded input and output, returning exact consumed/written
prefixes and NeedInput, NeedOutput, Yield or Complete. last marks EOF at the
end of the supplied slice; retain its unconsumed suffix and repeat last
until completion. Empty output is backpressure. After Complete, later polls
return Complete with zero consumption/output and no meter work, even if the
shared meter has since stopped. Use a fresh decoder for another body.
Encoding problems are recoverable diagnostics, provisional until Complete.
Padding ends the alphabet, but trailing bytes still require validation.

One poll performs at most 256 transitions. Each consumed octet, including
discarded bytes/whitespace and replay, charges Meter.io_bytes; each emitted
octet charges output_bytes before copying. EOF consumes one transition.
These fixed-cost byte transitions do not consume the examined-record
counter; enclosing MIME object traversal charges its own records. Meter
deadlines are checked at entry and every transition. The deterministic
decoder receives the coordinator's sampled Tick; the caller must bracket
turns with fresh clock and cancellation checks. A work/deadline refusal is
sticky in both decoder and meter, may leave partial caller output and must
not become a partial successful body.

Decoder is fixed, Copy state. A checkpoint must separately retain its source
position and enclosing job meter; restoring decoder state never refunds
charged work. Source extents, nested rings, filesystem reads,
QP/charset/NFC, part authorization and protocol output remain separate. No
body buffer, source reader or heap owner is stored in the decoder.

### 1.13 Bounded transfer input

mime_input::Reader borrows a live BlobReader, caller buffer of 1..=6144
bytes and checked offset/length. The caller must establish account/root/part
authorization and reproduce the exact MIME descriptor; a bounds check alone
grants none of those. Identity, base64 and quoted-printable are implemented.
Unknown transfer tokens use identity bytes with a diagnostic supplied by the
MIME parser. The source borrow retains the underlying body owner's pin. No
raw file accessor or complete body allocation is added.

Each poll performs either one source read of at most 6 KiB or one decoder
turn of at most 256 transitions; identity copies at most 256 bytes. Refill
returns Yield without decoding in that turn. Charge the entire requested
read capacity before I/O, including short or failed reads; charge each later
resident source visit and emitted byte again during decoding. Thus a full
successful read and its traversal consume two source-byte charges. NeedInput
is internal; public progress is Yield, NeedOutput or Complete with exact
written bytes. Identity returns Yield after a bounded copy while more bytes
remain, including when that copy fills the output; NeedOutput means its
nonempty input received an empty output slice. Base64 and QP can also return
NeedOutput after filling a nonempty slice with pending decoded bytes,
including a QP hard-CRLF or malformed-escape pair. In all cases the caller
consumes written bytes before polling again with available output. A short
successful read is allowed; zero progress before the declared extent end or
a count beyond the requested slice is Corrupt. Never probe outside that
extent.

QP Reposition maps the decoder's relative position to the checked absolute
extent cursor. If that cursor is still in the resident buffer, move only its
used cursor and return Yield; the next turn reuses those bytes. Otherwise
discard buffered source bytes, replace the physical cursor and yield before
the next refill. Replayed reads charge their full requested capacities;
resident visits are charged again in both paths. No manual checkpoint slot
is used by a raw-source QP rewind; saved caller checkpoints include the QP
mode/positions. The source borrow, monotonic watermark and live meter never
change. This implements one raw-source stage, not nested decoded-source
checkpointing.

One monotonic watermark and Meter deadline bracket every active turn. A
post-work clock/budget refusal overrides its earlier result; any failure is
sticky without later I/O or clock calls. Output and position may already
reflect work and remain provisional on error. Complete is retained only
after the final bracket succeeds and later polls return zero-progress
Complete without work until an earlier checkpoint is restored. position is
the current decoded cursor and rewinds on restore; replayed output still
consumes work. Encoding diagnostics describe that cursor's decoding history
and become final at completion. Invalid extents/backing/checkpoints, adapter
errors and work refusal remain distinct fixed errors. with_checkpoints
exclusively borrows the stage's caller-owned Checkpoints storage and clears
its eight private slots on every new binding. Save/restore use slot numbers;
save replaces that slot's earlier point. The caller owns slot assignment
across outstanding lookahead operations; nested slot scheduling is separate.
No saved state can be supplied from another source. Each slot retains the
exact consumed encoded cursor, decoded position, fixed transfer state
(including pending output/diagnostic and QP replay positions) and completion
flag. Save subtracts unread buffered bytes from the fetched cursor. Restore
discards the ring contents and refills from that cursor, never from the body
origin.

Checkpoint operations charge one record and bracket the bounded copy with
the same fresh monotonic/deadline checks. A post-copy clock failure overrides
an earlier slot error and retires the reader. Missing backing, unset/outside
slots and all work/clock errors retire it too. Restore retains the current
clock watermark, live job meter and every charge; it cannot revive a failed
reader or extend its lifetime. Even a completed checkpoint requires a live
restore operation before cached Complete can be polled. Source positions and
slots remain private to the borrowed reader; drop/rebind cannot carry a saved
point to a different body. Nested source chains and protocol output remain
separate.

### 1.14 Raw header scanner

mime_headers::Scanner consumes successive source chunks with an absolute
starting offset, caller-supplied remaining aggregate header allowance and
explicit EOF. It returns consumed bytes and one Field, Complete, NeedInput
or Yield. Field carries checked name/value half-open source extents; names
exclude obsolete whitespace before the colon, and values exclude their last
line ending while retaining folds and leading whitespace. No value decoding
or source access occurs. Caller input resumes at consumed, including after a
zero-consumption field event. Every emitted field is provisional until the
scan completes successfully. Once EOF is observed, later input is ignored;
a pending field event is followed by Complete at the retained EOF.

CRLF and bare LF end lines, including split CRLF; bare CR remains data.
Empty lines end headers. An invalid field or unattached continuation begins
the body at that line's start, even if tentative name bytes were already
consumed. Complete reports the authoritative body offset and recognized
header byte count; the input cursor alone is not a body boundary. EOF after
recognized headers yields an empty body. Headers include field endings and
folds, exclude the empty separator, and count exactly once. The caller owns
aggregate accounting across entities and raw source retention. A long
ambiguous name uses only scalar state and is subject to the work budget;
once its colon arrives the whole candidate is checked against the byte cap.

Each turn performs at most 256 transitions, emitting at most one field.
Charge every source lookahead, including one revisited after a field event,
and one record per emitted field. EOF checks consume no source byte but
still check the deadline. The caller brackets deterministic turns with fresh
clock/cancellation checks. HeaderLimit, offset overflow and work failures are
distinct and sticky; terminal completion is stable without later work. No
partial field list is a successful result after refusal. Header collection,
unfolding, Unicode and MIME tree integration remain separate.

### 1.15 Bounded charset decoding

mime_charset::Charset parses exactly POLICY.md's ten case-insensitive labels
for UTF-8, ASCII, Latin-1 and Windows-1252. The caller has already unquoted
the label; no whitespace trim or implicit unknown-label fallback occurs.
Decoder returns one Scalar, NeedInput or Complete with consumed source bytes.
Resume at the unconsumed suffix, including after zero-consumption replacement
of a prefix retained from an earlier chunk. UTF-8 replaces one maximal invalid
subpart at a time; incomplete valid prefixes become one replacement at EOF.
ASCII high bytes and undefined Windows-1252 bytes each become U+FFFD and set
the diagnostic. Latin-1 preserves C1 controls. Valid NUL and noncharacter
scalars remain intact here; form-specific filtering and I-JSON replacement
belong to projection.

A turn visits at most four source bytes and emits at most one scalar. Every
lookahead, including an invalid continuation revisited on the following turn,
charges source work; each scalar charges one record. Output serialization
charges its own bytes. The caller brackets deterministic turns with fresh
clock/cancellation checks. Work refusal is sticky. An observed EOF is retained
across its pending replacement; later input cannot extend that source.
Complete remains stable without later work. Copy state fits 32 bytes and may
join an owner-bound source checkpoint; restoring it never refunds the shared
work meter. Unknown-label policy, absent-charset UTF-8 prescan, encoded words,
NFC, source ownership and protocol projection remain separate.

### 1.16 Byte-preserving header unfolding

mime_unfold::Decoder removes CRLF or bare LF only when followed by SP/HTAB,
retaining that following whitespace. Bare CR, other line endings, leading
whitespace, NUL and all other octets pass through unchanged. This is one pass
over the original bytes: removal never reclassifies an earlier emitted ending
as a new fold. Field extents come from the header scanner; this primitive does
not parse fields or apply SMTP line framing, charset replacement, initial-SP
removal, encoded-word rules or NFC.

Progress uses consumed/written counts with NeedInput, NeedOutput, Yield or
Complete. Caller buffers may be empty and byte fragments may split either
ending. Retain the unconsumed suffix and last flag until completion. The fixed
state retains only a possible ending and at most two pending output bytes.
Every turn has at most 256 transitions. Charge all source lookahead, including
bytes revisited after flushing a nonfold ending, and each emitted byte. The
owning header traversal charges its records. The caller brackets deterministic
turns with fresh clock/cancellation checks; work refusal is sticky even with a
fresh meter, and completed state remains stable without work. A refusal may
leave partial caller output and unreported source progress; discard it and
never return a partial unfolded value as success. Copied state may later join
an owner-bound source checkpoint; restoration must retain the live meter and
owner retirement status, never revive a failed operation with fresh work.

### 1.17 Resident Raw header projection

header_raw::Cursor borrows a resident immutable header value, selected after
successful header scanning and aggregate header-arena admission. It decodes
UTF-8 with maximal-subpart replacement, then drops NUL and replaces Unicode
noncharacters with U+FFFD for I-JSON. Removing NUL never joins broken UTF-8
fragments. Preserve leading/trailing whitespace, folds, capitalization,
literal encoded words, valid unassigned scalars and decomposed text. Raw form
never unfolds, decodes encoded words or applies NFC.

Each poll returns one Scalar, Yield for a removed NUL, or Complete. It visits
at most four source bytes and inspects at most one decoded scalar. Charset
decoding charges source visits and one scalar record, including the fixed Raw
filter work and removed NUL. No second scalar record is needed; a maximal
ASCII header thus fits the default record allowance before other projection
work. Serialization charges output bytes.
position is the resident source-byte cursor. The encoding diagnostic records
malformed UTF-8/noncharacter replacement, is provisional until completion,
and does not flag the policy's NUL removal. A copied cursor retains the same
source reference; owner-bound restoration retains the live meter and must
never revive a failed operation. Work refusal retires the cursor even with a
fresh meter; discard provisional output. Completed state is stable without
further work. The owner brackets deterministic turns with fresh clock checks.
Source collection, header selection, JSON serialization and JMAP activation
remain separate.

### 1.18 Body charset selection and prescan

body_charset::Plan takes an optional, already-unquoted label. Absent and
ASCII labels require Prescan; other known labels select their exact decoder.
Unknown labels select UTF-8 replacement and mark an encoding problem. Keep
the original label for MIME properties; an absent property remains us-ascii
regardless of the selected decoder. Label syntax/admission belongs to the
parameter parser, not this selector.

Prescan reads the complete transfer-decoded body through caller fragments.
It selects UTF-8 only when every byte is valid UTF-8 and at least one scalar
is non-ASCII; otherwise select ASCII. A selection is available only at EOF,
never for a valid prefix. Promotion or malformed input records an encoding
problem. This diagnostic combines with transfer decoding, the final charset
decoder and JSON projection; it never clears another stage's diagnostic.
Explicit Latin-1/Windows-1252/UTF-8 and unknown-label fallback bypass this
heuristic. Prescan does not reject valid Unicode noncharacters; projection
replaces those separately.

Each poll inspects at most four bytes and one scalar, returning exact
consumed progress with NeedInput, Yield, or Complete(Selection). Retain the
unconsumed suffix and EOF flag on Yield. Charset decoding charges source
visits and one scalar record including the high-byte classification. The
final constant selection step checks the deadline without charging a scalar
record. Both prescan and final decoding consume the live work meter. The
owner retains the immutable source binding, restores its transfer source for
the final pass, brackets turns with fresh clock/cancellation checks and
preserves retirement across any saved state. Prescan owns neither a source
nor a replay buffer. Refusal is sticky even with a fresh meter and exposes
no selection; completed state is stable without work. MIME parameter
integration and body-value output remain separate; section 1.19 owns
transfer-source rewind.

### 1.19 Owned transfer-to-charset reader

mime_text::Reader constructs its private transfer Reader from one authorized
immutable Input extent, caller stage bytes and Checkpoints. Input's charset
plan comes from body_charset::Plan::from_label after parameter admission.
It retains the source borrow for its whole lifetime; the caller cannot
rebind its source or replace internal checkpoints. For absent/ASCII labels,
save slot zero at the start, prescan the complete decoded extent, restore
that exact start and decode again. Explicit selections use one pass. Both
passes charge the same live meter, including source reads, transfer work,
charset visits/scalars and checkpoint operations. Restore never refunds work
or resets an earlier transfer diagnostic.

One poll performs one transfer/checkpoint operation or one scalar turn,
returning Yield, Scalar or Complete. One pending transfer byte bounds staging
without another ring. There is no caller-output buffer or successful partial
text after an error. A private shared clock watermark covers outer and nested
samples; fresh pre/post samples bracket every turn, and a late clock/budget
failure overrides the result. Errors retire the owner even if retried with a
fresh meter; no further source or clock call occurs. Completed state is stable
without work. The selection is available after prescan (or at construction
for explicit labels) and is hidden after failure. The combined encoding
flag remains provisional diagnostic information on failure and is final only
at Complete. Nested errors name an operation: Input(Policy) can originate in
its clock or I/O adapter, so it is not proof of disk failure. A stopped meter
is reported as Work by the outer post-check when its clock sample succeeds;
a late clock failure still takes precedence.

Identity/base64/QP are supported through the existing transfer source. This
reader emits charset scalars, preserving NUL,
noncharacters and line endings. Body-value CRLF/JSON filtering, truncation,
MIME parameter integration and JMAP output remain separate. Live body/part
authorization belongs to the caller and is not granted by range validation.

### 1.20 Plain body-value filtering and byte caps

body_value::Plain consumes already-decoded scalars for non-HTML body values.
Convert CRLF to LF and replace Unicode noncharacters with U+FFFD, retaining
bare CR, LF, NUL, other controls, valid unassigned scalars and decomposed text.
This is the body policy, not Raw/Text header filtering. Never apply NFC or
HTML interpretation here. MIME/charset decoding remains in section 1.19;
JSON escaping and HTML truncation remain separate.

poll takes an optional scalar and a last flag, returning whether it consumed
the scalar with Scalar, Yield, NeedInput or Complete. Retain an unconsumed
scalar/last pair: flushing a preceding bare CR may consume neither. A consumed
last scalar or empty last input records EOF; drain a final CR before Complete.
A temporarily absent scalar with last=false does not finish a pending CR.
At most one input scalar is inspected and one output scalar emitted per turn.

The supplied cap counts projected UTF-8 octets, with zero disabling only this
argument's cap. Emit the longest scalar prefix within the cap; once the next
scalar will not fit, suppress it and all subsequent output. A value ending
exactly at the cap is not truncated. Continue consuming to actual source EOF
so noncharacters and upstream malformed tails still contribute diagnostics.
Combine the projection flag with transfer/charset flags. Metadata is final
only at Complete. A later source, work or count-overflow failure must discard
provisional output; truncation never grants partial successful validation.

Charge one record per scalar inspection, including a revisited lookahead after
bare CR, and each emitted UTF-8 octet. This standalone filter cannot assume
an upstream decoder charged scalar work. Composition with the charset reader
therefore charges two records per ordinary scalar, or three with prescan,
before other traversal work. The default two-million-record allowance can
limit body interpretation below the message byte limit; a small output cap
does not bypass complete-tail validation. Raw downloads remain available.
JSON serialization charges its own wire output. Caller-owned source/scalar
checkpoints retain the live meter and
retirement state; copied filter state never refunds work or revives failure.
The caller brackets deterministic turns with fresh clock/cancellation checks.
Refusals latch even with a fresh meter; Complete remains stable without work.
This primitive owns neither the source nor a response writer.

### 1.21 Stable quoted-printable cursor

mime_qp::Decoder implements POLICY.md section 3's stable QP octets using an
immutable source extent length and a body-relative position. Before each
poll, supply a fragment beginning at position(); bytes beyond that length
are ignored. The constructor binds only a length: the enclosing source
owner must preserve the same authorized bytes throughout decoding/replay.
This pure cursor has no source handle, authorization or I/O authority.

Progress returns consumed/written prefixes and NeedInput, NeedOutput, Yield,
Reposition or Complete. Except for Reposition, advance the supplied fragment
by consumed. On Reposition discard its remaining suffix and fetch from the
new position() before polling again; previously written bytes still count.
Rewinds begin at the exact literal whitespace/CR run, never at body origin.
Rewinds into the same supplied fragment reuse that resident prefix during
the current turn, retaining the 256-transition ceiling. Only an earlier
target returns Reposition. A fixed pending pair retains malformed escape
bytes or hard CRLF. Temporary
empty input below the extent end means NeedInput; EOF comes only from the
fixed length. Empty output is backpressure. Complete is cached without work.

Decode case-insensitive =HH; retain encoded spaces/tabs. Scan optional soft
break padding and literal whitespace to the next non-whitespace byte or EOF.
Drop trailing literal whitespace; replay interior whitespace once with its
original SP/HTAB ordering. Preserve hard line endings and malformed escape
bytes, diagnose tolerated LF/EOF soft breaks, bare CR/LF and prohibited
literal bytes. Flags are provisional until the full extent completes.

One poll performs at most 256 transitions. Every examined source byte,
including unconsumed lookahead and replay, charges io_bytes; each emitted
byte charges output_bytes before copying. Fixed transitions need no separate
record charge. EOF and active entry check the deadline. The caller brackets
turns with fresh clock/cancellation checks. Errors retire the cursor even
with a fresh meter and invalidate the whole provisional decoded body.
Copied state retains neither source identity nor meter: its owner must keep
both bindings and failure retirement, without refunds. Nested source
checkpoints and protocol output remain separate. The existing
mime_input::Reader binds this cursor to one immutable raw extent.

### 1.22 Fixed Unicode lookups

M06o supplies the Unicode 17 `unicode` module. `decompose(char)` returns a
copied `Decomposition` of one to four scalars, at most 20 bytes, exposed by an
exact-size iterator. The output recursively expands canonical mappings and
Hangul but does not reorder combining marks. Compatibility mappings remain
unchanged. `combining_class(char)` returns the pinned class or zero;
`simple_lowercase(char)` returns one pinned simple mapping or the input.
This is not full case folding, contextual casing or NFC.

`compose(left, right)` returns an eligible canonical table/Hangul composition
or None. Its caller must enforce canonical ordering and blocking, including
class-zero boundaries. All four functions return `InvalidTable` if a checked
compiled-table access or scalar conversion violates the generator contract;
unknown valid scalars receive the documented identity/zero/None behavior.
Private fixed storage cannot grow. These lookups do not allocate, fetch data,
keep mutable global state, sample a clock or change an admission meter. The
enclosing cursor charges each bounded operation and checks its deadline.

Sorted static tables bound binary searches independently of message length;
one decomposition emits at most four scalars. The lookup-specific official
corpus test covers all 11172 Hangul syllables; the resident NFC cursor below
owns the complete NFC equations.

### 1.23 Resident UTF-8 NFC

M06p supplies `nfc::Cursor` over one borrowed immutable valid UTF-8 `&str`.
It borrows one exclusive `Scratch`, the live admission `Meter` and an email's
aggregate `HeaderBudget`; none is copied into replay checkpoints. `poll(Tick)`
returns one normalized `Scalar`, `Yield` or `Complete`. It performs at most
32 state transitions and charges at most 128 header-budget steps per turn.
Callers supply fresh monotonic clock samples and bracket turns with
cancellation checks. `charge_output(tick, bytes)` charges serialized bytes to
the same borrowed meter before publication; a zero-byte charge checks the
post-turn deadline. Refusal retires the cursor even after its final scalar.
The cursor samples no clock and performs no I/O. Polling alone does not charge
`output_bytes` for scalar results.

Scratch is exactly 3072 bytes: 256 eight-byte scalar/class cells plus 256
u32 class counts. The cursor and aggregate budget together fit within the
remaining 1024-byte NFC checkpoint reservation. Each of four private source
copies retains UTF-8 position and up to four pending decomposed scalars.
Checkpoints compare identity and position without scanning source prefixes.
All fixed storage is caller-owned or inline; no admitted heap growth occurs.
The first 256 nonstarters use stable insertion ordering with one bounded
move per step. Longer segments replay once per occupied class in ascending
order, preserving source order within each class. Composition computes the
starter first and repeats decisions to emit any remaining marks. Class-zero
composition, including Hangul, continues across ordering boundaries.

`HeaderBudget::new()` sets 16 MiB source visits and 16000000 steps.
Reuse this one budget across all projections of an email's headers. Every
source scalar decode charges its UTF-8 bytes and two steps; each decomposed
scalar/class lookup charges one step; each state transition charges one.
Replay repeats these charges. Cached pending decomposition needs no byte
reread, but still charges each scalar step. Stable sorting, composition and
checkpoint transitions are charged even without a source byte. These charges
also debit the live meter's `io_bytes`. For its `records` counter, one
precharged record admits at most 16 internal steps. This private cursor
credit starts at zero and is never refunded or copied by checkpoints. Thus
a turn charges at most eight job records while the header budget retains
exact individual step accounting. A 1 MiB ASCII projection fits the default
2000000-record foreground budget; a hostile replay reaches its aggregate
interpretation limit under those defaults in the independent fixture.
Other work can consume the enclosing budget first; neither cap is bypassed.
A header-budget refusal is `InterpretationLimit` and
retires that budget across later cursors. Enclosing meter refusals are
`Work(Stop)`. Invalid private/table state returns a typed error. All cursor
errors are sticky and invalidate the entire provisional property, including
any scalar already returned. Cached completion does not recheck the clock.

This entry point accepts valid UTF-8 only; NUL, noncharacters and unassigned
scalars retain their Unicode normalization semantics. Section 1.27 adds a
separate decoded unstructured-header entry point. Callers must enforce the
admitted source extent. Protocol integration remains separate; Unicode
conformance and bounded replay do not establish whole-service memory.

### 1.24 Encoded-word candidate syntax

M06q supplies `encoded_word::Word::recognize(token, context, tick, meter)`.
The caller must first identify a complete token in a permitted lexical
position. `Context::{Text,Phrase,Comment}` selects the RFC 2047 payload
restrictions; it neither authorizes that position nor parses a whole header.
In particular, addr-specs, quoted strings and forbidden header fields cannot
be authorized by choosing a context. The future header cursor must enforce
placement, both surrounding whitespace boundaries and the header-form
whitelist before decoding. No convenient prefix of a token is accepted.

The recognizer accepts at most 75 ASCII bytes, exact delimiters, a known
charset and case-insensitive B/Q encoding. It requires nonempty printable
encoded text without whitespace or question marks. Q payloads apply the
additional phrase/comment character restrictions. Token and comment rules
include verified RFC 2047 errata
[504](https://www.rfc-editor.org/errata/eid504) and
[506](https://www.rfc-editor.org/errata/eid506). A supported charset's
optional RFC 2231 language qualifier is retained as a borrowed slice; its
lexical shape is a 1..8-letter primary tag followed by optional 1..8-letter/
digit subtags. No language registry, locale or language negotiation is used.
Ordinary charset labels still obey encoded-word token syntax: the known body
alias ansi_x3.4-1968 contains a forbidden period and remains literal here.

`Ok(None)` means retain the complete literal token. `Some(Word)` exposes only
borrowed payload/language and copied charset/encoding enums, at most 48 bytes.
It establishes candidate syntax, not payload decodability: bad Base64 or Q
escapes remain available to a separate replacement decoder. The recognizer
performs no decoding, control removal, whitespace suppression, NFC or output.
Those next layers must keep adjacent words' charset state separate.

Every attempt charges one record before the length guard. Candidates within
9 through 75 bytes additionally precharge three conservative candidate scans:
3 * length source bytes and records, even if syntax later fails. This bounds
an attempt to 226 charged records and two fixed slices plus enum state, with
no growing buffer or admitted allocation. One record covers bounded charset
lookup; the scan allowance covers delimiters, grammar and language shape.
Oversized candidates are rejected without reading their bytes. Work errors
propagate with the meter's sticky failure; no partial Word is returned.
Callers bracket attempts with fresh clock/cancellation checks and retain
aggregate header accounting when composing this helper with NFC.

### 1.25 Encoded-word payload decoding

M06r supplies `encoded_word::decode::Cursor` over one previously recognized
Word. Its constructor creates independent charset state. Poll returns at
most one Scalar, Yield or Complete. B and Q decoding follow POLICY.md's
explicit replacement/recovery rules, then use the fixed charset decoder.
Faults flush incomplete charset prefixes before replacement and resumption.
Encoded controls are removed after charset conversion; I-JSON noncharacters
become U+FFFD. `is_encoding_problem` accumulates transfer/charset/noncharacter
errors and is final at completion. Control removal alone is not an error.

The cursor is Copy and at most 128 bytes, including its borrowed Word,
three-byte Base64 pending buffer and charset state. Each poll charges before
work, at most three records and five byte visits: up to four payload bytes
and one charset-byte visit, or Q escape/lookahead and charset visits.
Checkpoint restoration repeats those charges; counters are never copied or
refunded. Work errors latch on the cursor even with a replacement meter.
An enclosing owner must retain failure retirement across cursor copies and
all header projections. Cached Complete is inert. The owner brackets turns
with clock/cancellation checks and charges UTF-8 output separately against
the same live meter. Poll itself emits scalars without output-byte charges.

No allocation, I/O, implicit locale or additional dependency is introduced.
The state fits within the existing decoder/checkpoint reservations; this is
not a composed-header memory or stack qualification. Lexical placement,
adjacent-word whitespace suppression, unfolding, initial-SP removal, NFC
and protocol output remain enclosing header responsibilities. In particular,
a caller cannot treat candidate recognition as placement authorization or
join charset bytes from separate words. Section 1.27 composes the resident
unstructured-header source with NFC.

### 1.26 Resident header Text decoding

M06s supplies the public `header_text::Cursor::new` entry point over one
immutable unstructured field value, excluding its final line ending. The
caller must authorize the field and form first. This entry point is not an
address/comment/parameter parser and cannot authorize structured encoded
words. M06bm adds a private grammar-selected constructor for Keywords and
List-Id Text; M06bn adds Content-Type and Content-Disposition with
comment-only placement. These are selected only by the header-value
coordinator (section 1.53).
All modes produce Text-form scalars before NFC; they provide no field
selection, whole JMAP property publication or serializer.

Each poll returns at most one Scalar, Yield or Complete. Unfold CRLF or LF
followed by SP/HTAB, preserving that whitespace; preserve other endings and
bare CR. Remove only initial SP from the unfolded stream. Decode literal
UTF-8 with maximal-subpart replacement, then remove NUL and replace I-JSON
noncharacters. Other literal controls remain scalars for later JSON escaping.
In the unstructured mode, candidate words begin at field start or after
SP/HTAB and must end at the next such boundary or EOF. A bad suffix, an
overlong token or a syntactic nonword stays literal, without accepting a
convenient encoded prefix. Structured modes enforce original phrase-word
spacing and alphabet; comment candidates may also be bounded by parentheses
or a whole quoted pair. Escaped UTF-8 characters and maximal-subpart repairs
finish before the following candidate begins. Source quotes, quoted pairs
and comment punctuation stay literal. List-Id stops word admission at its
first unquoted/uncommented opening angle. MIME parameter fields admit words
only inside original comments; quoted values and other tokens remain literal.
POLICY.md owns malformed display recovery and the per-field nesting limit.

Recognized words use section 1.25's independent per-word decoder. Retain
whitespace after a word as raw offsets while scanning for the next token.
Discard it only if that token is another recognized word; otherwise replay
and unfold the same whitespace, preserving trailing spaces/tabs. Recognized
malformed payloads still decode with replacement and participate in this
adjacency rule. Removing a NUL or other decoded control does not retroactively
create lexical boundaries or a new initial-SP trim opportunity.

The copied cursor fits 208 bytes, including its borrowed source and current
word decoder, within the 256-byte decoding checkpoint. It owns no
header string, candidate buffer or whitespace buffer. Candidate scans stop
after at most 76 raw bytes; an oversized token streams literally thereafter.
Whitespace scanning/replay advances one unfolded byte per poll. Recognition
is a separate bounded turn, charging at most 226 records/225 byte visits in
unstructured mode and 226 records/229 visits in structured phrase mode,
including the left byte and right fold/whitespace checks. Other turns charge
source/lookahead, charset and word work as performed. Structured literal
transitions also charge one step per consumed byte or repaired escaped
prefix.
Replay repeats those charges. Tests retain a one-MiB ASCII literal header
within the default two-million-record job allowance.

The cursor latches failures; copied checkpoints contain no meter and the
owner must retain retirement across them. The owner brackets turns with
clock/cancellation checks and charges output bytes separately. Cached Complete
is inert. The final diagnostic combines literal and word decoding problems.
Section 1.27 connects this cursor to NFC with aggregate header source/step
accounting and exact restart points. This standalone entry point uses only
the job meter. Worker stack and service activation remain unqualified.

### 1.27 Normalized header Text

M06t supplies `nfc::Cursor::from_unstructured_header(bytes, scratch, meter,
header_budget)`. It uses section 1.26's resident decoding before canonical
normalization, including composition across adjacent encoded words. The
caller must first authorize one unstructured field/form and exclude its final
line ending. Structured header parsing, header-form selection, provisional
property publication, JSON escaping and worker scheduling remain external.
The valid-UTF8 constructor retains section 1.23's original scalar semantics.
M06bm's private `from_header(bytes, grammar, scratch, meter, header_budget)`
adds the grammar-selected resident source behind section 1.53's property
owner; it supplies no independent field/form authority.

These constructors borrow the same exclusive Scratch, live job Meter and
aggregate HeaderBudget. Source checkpoints now hold either a valid UTF-8
position or the complete selected header decoding state, plus pending
canonical expansion. Each fits 256 bytes; the complete cursor plus aggregate
budget remains within 1024 bytes alongside the 3072-byte scratch. A checked
successful-turn ordinal identifies exact deterministic header state together
with source pointer/length and the selected grammar, without scanning any
prefix. Failed cursors are
retired before comparisons. Restore includes states inside a word, charset
sequence, candidate scan, whitespace replay or decomposition. EOF replay ends
at the before-turn checkpoint; resumption retains the consumed EOF state.

A private charging interface routes every source/lookahead, recognition and
charset charge through the aggregate header budget before work. Each header
poll and canonical decomposition also charges a step; each normalizer state
transition and decomposed scalar retains section 1.23's charges. Thus source
visits include decoding lookahead, charset-byte visits and repeated
candidate scans. Job records prepay these steps in groups of 16, with
private credit outside checkpoints; a recognition batch can prepay several
groups at once. No refund or copied credit bypasses either live budget.
Standalone decoder APIs continue to use the supplied job Meter directly.
Internal interpretation-limit errors remain distinct from malformed data and
enclosing job refusals.

Header normalization performs one engine transition per poll, at most 228
aggregate steps and 15 job records, within the 256-step fairness ceiling.
Valid UTF-8 retains its 32-transition/128-step/eight-record bound. Output is
charged through `charge_output` on the same borrowed meter; a zero-byte
charge checks the post-turn deadline. Clock/cancellation checks remain with
the caller. All failures retire the whole provisional property and aggregate
interpretation refusal retires that budget across later header projections.
The final `is_encoding_problem` combines literal/word/noncharacter diagnostics
across original scanning and replay. Completion is inert; output-charge
refusal still retires a completed cursor.

Fixtures cover cross-word canonical/Hangul composition, filtering before NFC,
long replay across word/decomposition checkpoints, exact no-prefix-rescan
visits, default-budget maximal ASCII, shared limits and deadline failures.
Allocation intervals include fast and overflow paths with output charging.
The complete official resident Unicode corpus remains the normalization
oracle. No new allocation, dependency or unsafe surface is introduced; these
checks do not establish whole-worker stack or service RSS qualification.

### 1.28 Header property selection

M06u supplies `header_property::Cursor` over one decoded, immutable JSON
property key and explicit Email or BodyPart context. Polling returns Yield
or Complete with a borrowed Property; Complete(None) leaves a non-header key
to the enclosing JMAP dispatcher. The parameterized grammar is
`header:NAME[:asFORM][:all]`, with exact-case prefix/suffix tokens and
printable ASCII field names excluding colon. No extra field-name length
limit is imposed. Raw and the last occurrence are defaults; `:all` retains
every occurrence in wire order when section 1.29 applies the selection. The
original requested key and its field capitalization are preserved.
Convenience aliases use the standard canonical field names, forms and last-
occurrence behavior only in Email context. BodyPart context recognizes
parameterized keys; an Email convenience alias returns None so the body-part
dispatcher can reject unsupported properties. The future JSON parser decodes
property keys in place in the admitted request arena before borrowing them
here; unescaping cannot expand their UTF-8 byte length. It must validate
JSON escapes and update token extents before sharing immutable keys. This
selector neither decodes JSON nor owns a second key copy.

Raw is permitted for every field. Other forms enforce RFC 8621 sections
4.1.2.2-7, including obsolete Resent-Reply-To and the RFC 2369 list fields.
Fields outside RFC 5322/2369 accept every form, including List-Id and MIME
fields. InvalidProperty reports malformed parameterized keys; ForbiddenForm
reports a syntactically valid but disallowed combination. The JMAP boundary
must map those user mistakes to invalidArguments for a read or
invalidProperties for structured creation. Work and InvalidState retain
their distinct resource/internal meanings.

Authorization of Text form does not establish an unstructured field grammar
or authorize encoded words inside parameters, quoted strings or addresses.
This selector does not invoke the unstructured decoder. The future value
parser must choose the correct field grammar. Section 1.29 supplies resident
field matching and last/all traversal. Source capture, null/empty absence
semantics, duplicate creation properties, creation placement restrictions
and JSON publication remain enclosing responsibilities.

The non-Copy cursor fits 128 bytes and a selected value fits 48 bytes; names
borrow the source or static alias table. Name scans visit at most 32 bytes
per poll. Prefix work, one alias/classification row, or a suffix of at most
23 bytes are separate bounded turns. Suffix validation prepays eight scans;
a poll charges at most 184 byte visits and 32 records, never output bytes.
All comparisons debit the live job meter first. Length-mismatched fixed-
table rows need no source-byte charge. Failure latches even with a fresh
meter; cached Complete is inert. Callers bracket active polls with fresh
clock and cancellation checks and charge subsequent scans/output separately.
Allocation probes cover long names, convenience aliases, all-occurrence
selection and syntax/form refusals. This does not qualify a complete worker
stack.

### 1.29 Resident header selection

M06v supplies `header_select::Cursor` over immutable resident source, its
absolute base offset, a header-byte allowance, a validated header Property
and explicit SourceEnd. Use Prefix for a captured message prefix and Eof
only when the owner has established actual entity EOF. A prefix ending
before the scanner finds the header/body boundary fails with sticky
Truncated. Source must contain the complete header section and separator, or
end at actual entity EOF. A truncated collection is not EOF. It may contain
body lookahead; traversal stops at the scanner's body boundary. Source
capture/admission remains the caller's responsibility; this API does not
read a file or allocate a header arena. The caller also retains aggregate
structural admission across entity headers.

The cursor composes the raw header scanner with bounded ASCII case-
insensitive comparisons. Each Match yields the original absolute Field
extents, without changing capitalization, folds, leading whitespace or value
bytes. All-occurrence requests yield matches in wire order; last-occurrence
requests retain only the most recent match and yield it after the scan ends.
An absent field yields no Match in either mode. The enclosing projection
maps that absence to null or an empty array and parses the requested form.
No value is decoded or serialized by this traversal.

Every Match remains provisional until Complete(End), including the last
match. On any later error, discard the entire provisional result. Header
limit/offset errors remain typed scanner failures; work refusals propagate
unchanged and all errors latch across replacement meters. Cached Complete is
inert. One poll performs either a scanner turn of at most 256 source visits,
a comparison of at most 32 bytes per operand (64 charged visits), or a final
delivery transition. At most one record and no output bytes are charged per
poll. The caller brackets active turns with clock/cancellation checks and
charges later value parsing and output independently.

The non-Copy cursor fits 384 bytes, including scanner, selected-property
references and two optional Field descriptors. It retains no list of fields
or copied names/values. Existing body-job parser state covers this inline
state; immutable source and request-name storage remain externally owned.
Fixtures cover duplicate/absent fields, original spelling, obsolete name
whitespace, folds, EOF/body boundaries, long-name comparisons, offset/work
refusal and provisional result retirement. Allocation probes cover last/all,
absence and long names. Whole-worker stack and source capture remain open.

### 1.30 Structured-header comments and whitespace

M06w supplies `header_cfws::Cursor` over immutable field bytes and a
starting resident offset. The source must end at the enclosing scanner
Field.value_end, excluding its final CRLF or bare LF; never pass the
remaining message or a subsequent field. This lexical helper does not
discover field boundaries. The caller invokes it only where its structured
field grammar permits optional CFWS, never inside a quoted string or
arbitrarily across a field. It skips SP/HTAB and CRLF or bare-LF folds
followed by SP/HTAB under the raw-file policy. The first non-CFWS byte is
left untouched. Complete returns that position and whether any CFWS was
consumed, allowing the enclosing grammar to require a nonempty run where
appropriate.

Top-level comments yield their original start/end offsets including the
outer parentheses; nested comments stay inside that extent. Quoted pairs
suppress delimiter interpretation, including RFC 5322 obsolete ASCII quoted
controls and RFC 6532 UTF-8 characters. Unescaped obsolete comment controls
are accepted; unescaped NUL, bare CR, nonfold LF/CRLF, invalid/truncated UTF-8
inside comments, an unfinished escape or an unclosed comment are Malformed.
A non-CFWS byte outside a comment is left to the enclosing grammar.
Comment depth includes the outer pair and is capped at 32; excess depth is
NestingLimit, distinct from malformed syntax or work refusal. Non-CFWS text
is not Unicode-validated by this helper.

Comments are provisional until the optional run completes; a later refusal
invalidates earlier events. The form parser maps Malformed to strict null or
its specified syntax-recovery policy. NestingLimit remains an interpretation
refusal and work failures retain their resource meaning, never a fabricated
empty value. This helper neither decodes quoted pairs or encoded words nor
authorizes their placement, and it emits no display name, normalized text or
JSON. The raw source remains immutable. Source capture, complete field
admission and aggregate enclosing-parser limits are external.

A poll advances at most 32 transitions, charging at most 32 records and 160
byte visits before inspection. UTF-8 validation charges its bounded reread
of the lead octet; folding lookahead is charged even when it cannot fold.
No output bytes are charged. Failures latch with a replacement meter and
cached Complete is inert. Callers supply fresh clock/cancellation checks
around active turns. The non-Copy cursor fits 64 bytes in the existing body
parser reservation, with a depth counter and escape flag instead of a
recursive call stack. It owns no source copy or comment list. Allocation
intervals cover long Unicode comments, folds, nesting, empty matches and
syntax/depth refusal. Full structured forms and worker integration remain
open.

M06bp moves the lexical state into std-only td-header. The public mail
facade retains its existing errors, source authorization, position and
polling contract. A private adapter binds each shared admission call to this
turn's original job/email work owner and Tick; it adds no visits, records,
steps or retained state. Shared result extents/status are re-exported. The
shared cursor stores the original Copy admission error, so replacing the
adapter cannot revive failure. td-header/DESIGN.md owns the common syntax and
bounded callback contract; mail placement and protocol policy remain here.

### 1.31 Resident date-time interpretation

M06x supplies `header_date::Cursor` over one complete immutable field value,
excluding its final line ending. The caller first admits that field and
its requested form. The cursor composes CFWS with fixed token/grammar state
and returns Complete(Some(Date)) only after the entire value, including any
trailing comments, validates. Empty, malformed or out-of-range input returns
Complete(None). No accepted date prefix escapes before trailing validation.
Comment nesting and work exhaustion remain typed errors, never null. Both
completion and failure are sticky; cached results consume no additional work.

Date contains year, month, day, hour, minute, second and an Offset: known
signed minutes east of UTC or Unknown. These are copied interpretation data,
not a capability or an encoded JMAP Date. RFC 5322 sections 3.3 and 4.3 govern
syntax and semantics: English month/weekday names compare without ASCII case,
optional weekdays must agree with the Gregorian date, day and clock ranges
are checked, and omitted seconds become zero. Seconds through 60 are retained
as the RFC mail grammar permits; this parser does not consult leap-second
announcements or claim to validate an RFC 3339 instant.

Two-digit years 00..49 map to 2000..2049, 50..99 to 1950..1999; three-digit
years add 1900. Four or more digits mean the literal year, including leading
zeroes. The supported interpreted range is 1900..9999. Obsolete optional
CFWS and adjacent year/hour digits are accepted: when the next token after
optional CFWS is a colon, the last two digits of a combined year/hour run
are the hour and the preceding at-least-two digits are the year. The raw-
file bare-LF fold policy applies.

Numeric zones require trailing FWS before their sign and exactly four
unseparated digits. Offset minutes must be below 60; RFC mail offset hours
may reach 99, unlike RFC 3339's 23-hour ceiling. -0000 stays Unknown; +0000,
UT and GMT are known zero. The eight US standard/daylight spellings have
the RFC offsets. Single military letters other than J, including Z, and
unknown multi-letter alphabetic zones become Unknown under the RFC's
recommendation. No locale, OS time zone, DNS or external data is consulted.

A poll either advances one CFWS turn plus one charged delimiter-state visit,
or inspects at most 32 token bytes/EOF positions and performs one bounded
grammar transition. Its ceilings are 161 source-byte visits and 33 records,
with no output bytes. Revisited token delimiters and the trailing-FWS check
are charged. The non-Copy cursor fits 192 bytes, including inline CFWS state,
a three-byte word prefix and scalar counters. Long comments, zone names and
zero-prefixed years use constant storage. Source capture remains external;
section 1.54 adds aggregate interpretation admission. Section 1.32 projects
ordinary dates and pinned leap insertions; section 1.56 adds Date property
JSON. The JMAP method adapter remains follow-on work.

### 1.32 Date projection

M06y supplies `header_date::project::render` over copied Date components,
caller output storage, a tick and the live job meter. It revalidates the
public components before arithmetic. Ordinary seconds 00..59 produce an
RFC 3339 string with mandatory whole seconds and uppercase T/Z. Known
offsets normalize to UTC Z, including mail offsets through +/-99:59.
Unknown offsets keep the original clock and -00:00. No local time-zone
setting or native calendar routine participates.

The formatter accepts Gregorian component years 0000..9999; the mail parser
still accepts interpreted source years only from 1900. Offset normalization
may cross a month, leap day or year, but cannot leave the four-digit output
year range. Invalid public components or unrepresentable normalization
return OutOfRange without output. At most five day transitions are required
by the supported signed-minute offset range. Arithmetic on these bounded
components cannot wrap; year boundary transitions use checked operations.

M06ab qualifies second 60 using the operator-approved IANA source pin in
../td-mime/leap-seconds/README.md. Offline tooling verifies the exact source and derives
27 positive insertion dates; only the generated 108-byte table enters the
runtime. After validating raw components, normalize the offset without
changing the second. Known offsets become UTC; unknown -0000/obsolete zones
retain their UTC clock by RFC 5322 convention and the -00:00 marker. Only
23:59:60 on a listed UTC date produces Date output. No clamping or rollover
repairs an unqualified leap component.

Unlisted second-60 values return LeapSecondUnverified with no output,
including pre-1972/future dates, wrong UTC minutes and normalization outside
the output year range. Invalid raw components still return OutOfRange. The
unverified outcome is distinct from a malformed date; the eventual adapter
uses POLICY.md's null/diagnostic mapping. Source update/expiration metadata
is provenance, not runtime admission: expiration never invalidates a known
historical insertion or proves absence of a future announcement. There is
no runtime source file, timezone database, network lookup or automatic update.
Negative/discontinuous transitions refuse cold generation until supported by
a reviewed extension. Ordinary seconds retain their calendar/offset checks;
this does not predict future negative leaps.

Successful output occupies exactly 20 bytes for Z or 25 for -00:00 and
borrows the caller buffer; it includes no JSON quotes or fractional seconds.
The bounded TextBuffer formats only checked integer components and a static
suffix. Eight records prepay component validation, up to five date steps,
and fixed-width formatting. Valid raw second-60 components prepay another
six records for key construction and at most five binary-search comparisons
over at most 31 dates. No mail-source bytes are visited. Capacity is checked
before charging the exact output byte count, and that charge precedes all
writes. Capacity, work, OutOfRange and LeapSecondUnverified leave the buffer
unchanged; an internal formatting/state failure requires discarding output.
The caller propagates a failed live meter and retires its enclosing job;
this stateless helper cannot own that lifetime or prevent supplying a new
meter. JSON framing/publication needs its own separately charged bytes.

No string, timezone table or calendar object is allocated. Tests cover
literal mail-to-RFC output, nonhour offsets, five-day shifts, leap-century
boundaries, both four-digit year edges, malformed public components,
capacity/work refusal, all pinned insertions and the unverified leap
outcome. The Rust allocation interval exercises successful formatting and
refusal paths. The complete Date/JMAP adapter and worker stack remain
unqualified.

### 1.33 Delimited structured-header tokens

M06z supplies `header_delimited::Cursor` with explicit QuotedString or
DomainLiteral kind, immutable field source and a starting offset at the
opening delimiter. Source must end at the enclosing scanner value_end,
excluding its final CRLF or bare LF. Optional surrounding CFWS is handled
separately by the enclosing grammar. A successful token returns its original
start/end offsets, including delimiters, and leaves all following bytes
untouched. Complete means this token validated, not that its placement or
the rest of the field is valid. Enclosing form results stay provisional.

Quoted pairs suppress delimiter interpretation. Within strings, parentheses,
brackets, commas and address punctuation are ordinary data. Within a domain
literal, an unescaped opening bracket is malformed; escaped brackets and
quotes are data. Both kinds accept RFC 5322 obsolete controls/quoted pairs
and RFC 6532 UTF-8. SP/HTAB and accepted CRLF/bare-LF folds remain unchanged.
Unescaped NUL, bare CR, nonfold LF/CRLF, invalid/truncated UTF-8, a missing
opening delimiter, unterminated escape or missing close is Malformed.
Noncharacters remain valid lexical scalars; JSON projection handles them.

No unquoting, unfolding, encoded-word decoding, NFC, domain/IP validation
or recipient authorization happens here. This primitive grants no SMTP
syntax permission. A form parser maps malformed tokens to its strict null
or documented recovery policy; work refusal retains its resource meaning.
Failures latch across replacement meters. Cached completion is inert.

Each poll executes at most 32 transitions, charging at most 32 records and
160 source-byte visits before inspection. UTF-8 validation charges its lead
reread; fold lookahead is charged even when malformed. No output bytes are
charged. The non-Copy cursor fits 64 bytes in the existing body parser
reservation, with no recursion, copied token or growing delimiter state.
Callers retain field admission, aggregate work and fresh clock/cancellation
checks around turns. Fixtures cover raw extents, escapes, obsolete controls,
Unicode, exact charges, long inputs, scanner-delimited field boundaries and
sticky failures. Allocation intervals cover both kinds, long input and
malformed/work refusal. Complete address/MessageIds grammars and worker
integration remain open.

M06bp similarly moves delimited-token state into td-header and preserves
this mail facade. Kind, Extent and Status are shared; the private admission
adapter and error mapping keep the original mail resource/refusal semantics.
The fixed 64-byte and 160-visit/32-record bounds remain unchanged.

### 1.34 Resident MessageIds list grammar

M06aa supplies `header_message_ids::Cursor` over an admitted immutable field
value ending at scanner value_end, excluding its final line ending. Strict
mode parses a nonempty list of RFC 5322 msg-id values including their obsolete
local-part/domain grammar. ObsoletePhrases additionally accepts and discards
obsolete phrases for References and In-Reply-To. The enclosing header owner
must authorize that mode; it is not a recovery switch for other fields.
Phrase-only and truly empty obsolete fields yield an empty list; Strict
requires at least one msg-id. CFWS-only fields remain malformed in both modes
because the obsolete phrase grammar still requires an initial word. A leading
dot cannot begin an obsolete phrase. This is the JMAP read form's list grammar:
a malformed multi-ID Message-ID can still expose multiple parsed IDs, as
RFC 8621 section 4.1.3 permits. Outgoing field cardinality is separate.

Begin and End delimit one provisional message-id. Part returns an absolute
byte extent in the supplied field slice. Concatenating those parts produces
raw identifier source preserving local/domain spelling, dots, the at sign,
quoted strings and domain literals while omitting grammatical CFWS and the
outer angle brackets. Quoted pairs, interior whitespace/folds and obsolete
controls are retained as source bytes. Before identifier comparison or JMAP
projection, the owner must unfold CRLF or bare LF followed by SP/HTAB inside
parts, preserving the following whitespace as data. The existing mime_unfold
decoder supplies this byte-preserving stage; folded and unfolded forms must
compare equally. Valid RFC 6532 UTF-8 is accepted in atoms and delimited text.
Comments use the existing 32-level bound. There is no case folding, encoded-
word decoding, NFC, semantic unquoting or DNS/domain/IP validation.

All events remain provisional until Complete validates the entire field.
Malformed trailing values or comments invalidate every previously yielded
message-id. Malformed means the eventual MessageIds form is null; nesting
and work limits retain their resource-failure meaning. The eventual response
owner must validate before publishing and charge any replay against the same
live job. Section 1.35 supplies unfolding and scalar projection; raw extents
still need separately charged copying/serialization. They are not
publishable JSON or SMTP recipients. A completed field is inert; every error
remains sticky even if a caller supplies a replacement meter.

A poll performs one CFWS or delimited-token turn, one grammar transition, or
at most 32 atom visits. The maximum is 160 source-byte visits and 32 records,
including UTF-8 lead rereads, delimiters and CFWS reinspection. No output
bytes are charged because only source offsets are returned. State is non-
Copy and fits 256 bytes; no string, token list or recursion is allocated.
Whole-field admission, response lifetime, JSON framing and service integration
remain external. Fixtures pin literal projected byte segments, obsolete
phrases, Unicode, comments, malformed tails, exact charges, long atoms,
nesting/work refusals and allocation-free success/refusal paths.

### 1.35 Validated MessageIds text

M06ac supplies `header_message_ids::project::Cursor` over the same admitted
immutable field slice and explicitly authorized grammar mode as section
1.34. A complete syntax pass discards all raw events before restarting the
parser on that unchanged source. Replay uses the same caller meter. Thus
malformed tails, invalid UTF-8 and nesting refusal precede any Begin,
Scalar or End event. Empty obsolete lists complete without identifier text.

Replay emits Begin/Scalar/End for each identifier. Each raw part passes
through the existing byte-preserving unfolder and UTF-8 decoder using a
fixed one-byte handoff. CRLF or bare LF followed by SP/HTAB is unfolded;
interior whitespace, quoted pairs, quotes and domain brackets are retained.
Unicode noncharacters become U+FFFD and set is_encoding_problem, final at
Complete. NUL and other valid obsolete quoted controls remain scalars;
the eventual JSON serializer must escape them. No NFC, case folding,
encoded-word decoding, semantic unquoting or recipient validation occurs.

Each active poll prepays one parent record and performs at most one child
turn. The bound is 256 source/intermediate byte visits, 33 records and four
output bytes per poll. Both full grammar passes and intermediate UTF-8
byte consumption are charged. Output work includes unfolded intermediate
bytes plus final scalar UTF-8 lengths; JSON quotes, escaping, arrays and
framing require separate charges. For `<a@b>`, both syntax passes visit 28
bytes; unfolding/decoding visit six more and charge six output bytes.

Long ASCII atoms cost about 5.06 records per source byte, including the
two syntax passes and byte/scalar handoff, plus field/token overhead. A
256 KiB atom uses roughly two thirds of the default two-million-record
foreground allowance; a permitted 1 MiB header does not necessarily fit
that allowance. Field admission never promises successful projection under
a job's remaining work. The owner must account for all selected fields and
forms; exhaustion remains a resource refusal, preserving raw mail.

The non-Copy cursor fits 384 bytes including the parser and conversion
states; no identifier string or list is allocated. Every error latches
across fresh meters and cached Complete is inert. Work failure can still
follow text emission, so this helper grants no response publication or
capacity authority. The enclosing owner retains admission, live aggregate
work, clock/cancellation checks and reserved response lifetime. Tests cover
folds, obsolete controls, all Unicode noncharacters, unchanged decomposed
text, whole-field refusal, exact charges, long inputs and allocation-free
success/refusal. Complete JMAP serialization and worker qualification remain
open.

### 1.36 Resident URLs form

M06ad supplies `header_urls::Cursor` over the admitted immutable field slice
ending at scanner value_end. Mode URLs requires one or more comma-separated
angle-bracketed URLs with optional surrounding CFWS. Mode ListPost also
accepts exact `NO` with surrounding CFWS as an empty list; the property
owner must authorize that mode only for List-Post. No leading, trailing or
repeated comma is accepted. Malformed comments, URLs or trailing text
invalidate the entire field, with no prefix recovery. This deliberate
strict choice follows POLICY's null-on-parse-failure contract rather than
RFC 2369's SHOULD-ignore-tail client recovery. Raw remains available.

A complete charged validation pass precedes Begin/Byte/End events during
charged replay of the same source. The two passes use the same live meter.
Outer comments and brackets are removed. Within brackets, SP/HTAB and
CRLF or bare LF followed by SP/HTAB are discarded, including within scheme
names and percent escapes. Other raw line endings and controls refuse.
Parentheses inside a URL are URI data, not mail comments. Output is ASCII
and preserves scheme case, host case, percent spelling and dot segments.

The private validator checks the RFC 3986 URI grammar, including scheme,
authority/userinfo/port, path, query, fragment and percent escapes. Unknown
schemes and empty generic components are legal; scheme-specific usability
is left to clients. Ports have digit syntax without a numeric range check.
An invalid IPv4-looking name may still be a generic reg-name. IPv6 literals
use a fixed 45-byte local buffer and std's Ipv6Addr parser; IPvFuture uses
fixed scalar state. Scoped IP literals and IRIs are outside this grammar.
No URL is decoded, normalized, resolved, fetched or treated as authorization.

Every active poll charges one parent record and at most one CFWS turn or
one syntax byte. Source lookahead is charged before inspection; closing an
IPv6 literal prepays 64 additional records for the bounded local std parse.
Per-poll ceilings are 160 source-byte visits, 65 records and one output byte.
Both passes visit all required input and repeat literal verification. Output
work counts only replayed URL bytes; JSON framing/copying/escaping is the
response owner's separate responsibility. `<x:>` costs ten visits, eighteen
records and two output bytes; each IPv6 literal adds 128 records across both
passes. Long URL data costs about two records per raw byte plus list, CFWS
and literal overhead. Permitted header size does not guarantee fitting a
job's remaining aggregate work.

The non-Copy cursor fits 256 bytes including CFWS, URI and literal state.
No list or URL string is allocated. Malformed maps to the future form's null;
nesting and work refusal retain their resource meaning. All errors latch
across fresh meters; cached Complete is inert. Replay can fail after emitting
text, so admission, cancellation/clock checks and reserved response lifetime
remain external. Tests cover URI grammar, folds, special NO, long data,
whole-field refusal, exact costs and allocation-free success/refusal. JMAP
serialization and complete worker qualification remain open.

### 1.37 Address recovery item boundaries

M06ae supplies `header_address_items::Cursor` over an admitted immutable
field value ending at scanner value_end. It partitions raw source at commas
and semicolons outside quoted strings, comments, angle brackets and domain
literals. Quoted pairs protect the next byte only inside quotes, comments
or literals. Parentheses/quotes inside a literal are data; brackets/quotes
inside a comment are data. Nested comments and malformed nested angles
have separate 32-level counters. Colon stays in its raw item for the future
group grammar to interpret. This primitive validates no address syntax,
UTF-8, fold or escape, and grants no SMTP recipient authority.

Item gives absolute start/end offsets excluding the separator, its Comma,
Semicolon or End identity, and an unclosed flag. Every item preserves all
raw whitespace and bytes. Empty input and trailing separators return a
final empty End item once; consecutive separators retain empty items.
Those are grammar inputs, never automatically recovered empty addresses.
The future parser must track groups across items and distinguish group
closing semicolons from recovery separators.

An unclosed quote/comment/angle/literal consumes the remaining field item
and marks its End item unclosed; later commas cannot reopen recovery inside
that tail. A false unclosed flag does not prove valid address syntax. Stray
closing delimiters and other malformed bytes remain available to the
fallback parser. This bounded best-effort choice preserves POLICY's rule
against searching without limit for a missing closer. Complete reports
partition completion only. Item events can precede a later work/depth
failure and confer no response publication permission.

Each poll performs at most 32 byte/EOF steps, prepaid by one record and
one source-byte visit when a byte exists. A complete scan of n bytes costs
exactly n visits and n+1 records, including the final EOF item; offsets
charge no output bytes. Work/depth failures remain sticky across fresh
meters; cached Complete is inert. State is non-Copy and fits 64 bytes with
no token/string/list allocation or recursive stack. Admission, fresh clock
and cancellation checks, fallback decoding/NFC, group/name/mailbox parsing
and JMAP serialization remain with the enclosing owner. Tests and allocation
intervals cover protected delimiters, escapes, empty items, unmatched tails,
raw invalid bytes, long input, depth and sticky resource refusal.

M06ai also retains the first unprotected colon offset in each boundary
Item. Colons do not split items, and this metadata does not validate a group
name. Protected route/quoted/comment/literal colons never become candidates.
The added scalar state stays within the same 64-byte cursor ceiling and
does not change byte/record charges.

### 1.38 Resident single addr-spec grammar

M06af supplies `header_addr_spec::Cursor` over one admitted immutable
candidate slice. It enters the existing identifier parser's private bare
address mode: start at local-part and require EOF after a complete domain
and optional CFWS. The public MessageIds list constructor continues to
require its enclosing angles and list grammar. No synthetic input bytes or
second local-part/domain implementation is introduced.

The parser accepts RFC 5322 addr-spec including obsolete local-part/domain
CFWS placement, quoted local words and domain literals, plus RFC 6532 UTF-8.
It returns provisional Part extents in the candidate slice. Concatenating
parts preserves local/domain spelling, dots, at sign, quotes, quoted pairs,
folds and literal brackets while removing grammatical CFWS. It does not
unfold, unquote, normalize, decode encoded words or validate DNS/IP syntax.
Noncharacters and valid obsolete quoted controls remain lexical data for
later projection. Outbound SMTP validation is separate and stricter.

Complete requires the entire candidate to be one addr-spec. Display names,
outer angles, obsolete routes and comma/semicolon lists belong to the
mailbox/group grammar and are rejected here. Invalid tails invalidate all
provisional parts. The enclosing best-effort parser may recover Malformed
as POLICY's raw item fallback; nesting and work errors remain resource
refusals. Completion never authorizes a recipient or response publication.

The wrapper uses the same charged turns as the identifier core: at most
160 source-byte visits and 32 records per poll, no output bytes. Literal
a@b costs nine visits and twelve records; its four-byte local scalar
counterpart costs thirteen visits and twelve records. The non-Copy wrapper
fits 288 bytes including its private error latch; existing MessageIds
state stays within 256 bytes. Both share fixed CFWS/delimited/atom state,
with no new arena or copied address. All errors latch across fresh meters;
cached Complete is inert. Tests cover bare/enclosed grammar agreement,
quoted/obsolete/Unicode data, long atoms, precise work and whole-candidate
refusal; allocation intervals include success, malformed tails and resource
failure. Admission, address/group assembly, decoding/NFC, JSON serialization
and complete worker qualification remain open.

### 1.39 Resident phrase token grammar

M06ag supplies `header_phrase::Cursor` over one admitted immutable phrase
slice, using the identifier core's private phrase purpose. That purpose
reuses obsolete-word syntax for display names; public ObsoletePhrases
selection remains restricted to References and In-Reply-To. Public MessageIds
and single addr-spec semantics remain unchanged. Accept RFC 5322 phrase and
obsolete phrase syntax plus RFC 6532 UTF-8: at least one atom or quoted word,
then words, obsolete dots and optional CFWS. Empty and CFWS-only input,
leading dots, enclosing angles, address/group separators and malformed tails
refuse. Empty quoted words remain valid lexical words.

Each Token returns its Atom/Quoted/Dot kind, exact raw text extent and exact
leading CFWS extent. Complete returns the trailing CFWS extent. Offsets are
relative to the supplied slice. Tokens retain quotes, quoted pairs, folds,
noncharacters and valid obsolete quoted controls. Adjacent words can have
empty gaps. Tokens are provisional until the entire phrase completes;
Malformed invalidates them all. No text is copied, decoded, unquoted,
unfolded, normalized, serialized or authorized for publication here.

Exact CFWS is retained for later RFC 2047 placement: a comment-only gap is
not linear white space. An encoded-word-looking atom remains literal here;
quoted tokens never authorize encoded-word recognition. The later decoder
must inspect charged raw gaps and token boundaries, and the enclosing
mailbox/group parser must supply context outside the slice at `<` or `:`.
A nonempty gap alone never authorizes an encoded word.

The non-Copy cursor fits 320 bytes including the shared core and error latch.
Each poll visits at most 161 source bytes and charges at most 33 records:
core grammar work plus one prepaid byte/record to classify an emitted token.
Raw extents charge no output. A single ASCII letter costs four byte visits
and seven records; one four-byte scalar costs eight/seven. All errors latch
across replacement meters; cached completion performs no work. Tests cover
literal token/gap boundaries, encoding context, obsolete and Unicode data,
long tokens, complete-input rejection and exact/sticky resource refusal.
Allocation intervals cover success and malformed/nesting/work failures.
Mailbox/group assembly, display decoding/NFC, JSON output and composed
worker resource qualification remain open.

### 1.40 Resident single mailbox grammar

M06ah supplies `header_mailbox::Cursor` over one complete admitted item,
excluding its enclosing list/group separator. Complete returns one Mailbox
with a raw address extent and optional Name::Phrase or Name::Comment extent,
all relative to the original slice. There are no provisional public events:
syntax, prefix, route, address and trailing CFWS must all finish first.
Malformed permits the outer best-effort owner's raw fallback; work/nesting
errors retain resource meaning. No result authorizes an SMTP recipient.

A bounded structural scan uses shared CFWS and delimited-token cursors to
protect punctuation inside comments, quoted words and domain literals.
Accept bare addr-spec or one optional phrase plus angle-enclosed addr-spec;
reject multiple/nested/unmatched angles, list/group separators and extra
tail text. Validate a CFWS-only prefix separately from a nonempty phrase.
The phrase extent retains all prefix CFWS, including the raw boundary at
`<` needed by later encoded-word placement. The enclosing list/group owner
must also preserve context before this slice.

Inside angles, accept RFC 5322 obsolete routes only after validating the
complete obs-domain-list: leading CFWS/empty comma slots, at least one
@domain, then comma-separated optional CFWS/@domain slots, and a colon.
A private shared domain entry stops at an unconsumed comma or the admitted
route boundary; it does not loosen public MessageIds or addr-spec parsing.
Only the validated route is omitted from the returned address extent.
Domains remain lexical data without DNS/IP validation or network lookup.

A phrase name takes precedence, including an empty quoted word. Otherwise,
replay CFWS after the last actual addr-spec part and select its first
comment, retaining surrounding parentheses and any nested comments. This
fallback applies to a bare address or a comment before the closing angle;
a comment after `>` is outside addr-spec and is not selected. Leading,
interior-domain and route comments are never substituted as names. The
address extent retains grammatical CFWS; the later projector must replay
addr-spec parts to omit it, rather than emitting the entire raw slice.
No unquoting, unfolding, encoded-word interpretation, NFC or JSON emission
occurs in this grammar layer.

Child state is held in one enum, with no address/name copy or arena. The
non-Copy cursor fits 512 bytes. One poll prepays one parent record and
performs at most one bounded child turn, for ceilings of 161 source-byte
visits and 34 records, with no output charge. A bare a@b costs twelve
visits and twenty-eight records including its structural scan and trailing
comment replay. All failures latch across replacement meters; cached
Complete is inert. Tests cover literal name/address extents, protected
delimiters, route grammar, fallback precedence, invalid tails, long Unicode,
exact work and sticky failures. Allocation intervals include success and
malformed/nesting/work refusal. List/group recovery, text/NFC projection,
response admission and complete worker qualification remain open.

### 1.41 Resident address/group assembly

M06ai supplies `header_addresses::Cursor` over one complete admitted field
value. It emits provisional BeginGroup(optional raw phrase extent), Mailbox
(Parsed mailbox or Raw fallback extent), EndGroup and Complete events.
All extents use the original field's offsets. Named groups retain their
names, including empty quoted phrases. Consecutive ordinary mailboxes share
an unnamed group; empty/null slots and CFWS-only items produce no mailboxes.
Addresses can flatten these events, while GroupedAddresses retains them.
Every event is provisional until whole-field Complete; a later resource
failure retires the entire result. This layer emits no JSON or strings.

The boundary scanner retains the first colon outside comments, quotes,
domain literals and angles. Outside a named group, a fully valid nonempty
phrase before that colon starts a named group. Parse the suffix of that
same item as its first mailbox, if nonempty. A following semicolon closes
the group, including an empty group. Inside a named group, a further colon
never starts a nested group. Invalid group-name syntax leaves the entire
item for raw fallback. Work/nesting failures never select recovery.

A missing named-group semicolon is recovered by an implicit close at EOF.
A stray semicolon is a recovery boundary: process its preceding item, close
any unnamed run and ignore an empty item. Start a later unnamed run only
when another mailbox exists. This is deliberate best-effort recovery,
not acceptance of those forms as RFC grammar. Commas/null slots do not
split an existing unnamed run; explicit named groups and semicolons do.

Complete single-mailbox parsing supplies Parsed offsets and names. Ordinary
malformed items become Raw with no display name. Before emitting Raw, trim
only leading/trailing ASCII SP, HTAB, CR and LF with charged bounded steps;
a resulting empty span is ignored. Keep internal folds, invalid UTF-8,
NUL and other bytes unchanged for later unfolded/filtered projection.
Unclosed constructs consume the boundary scanner's remaining field tail.
Raw or Parsed status never grants SMTP delivery authority. The enclosing
owner must retain delimiter context before/after name extents for later
encoded-word placement, then decode/normalize/project before publication.

The non-Copy cursor fits 768 bytes including the 64-byte boundary cursor,
one enum-held child, group state and pending extents/event. No growing item
or group collection is stored. Each poll prepays one parent record and runs
at most one child turn: at most 161 source-byte visits, 35 records and no
output charge. Raw trimming charges one visited byte per active turn.
Failures latch across fresh meters; cached Complete is inert. Tests and
allocation intervals cover named/unnamed transitions, empty groups/null
slots, routes, comments, invalid names/nesting, malformed and unclosed
tails, long Unicode and sticky resource refusal. Text/NFC projection,
response admission and complete worker qualification remain open.

### 1.42 Resident address text projection

M06aj supplies `header_address_text::Cursor` with Parsed and Fallback modes
for admitted immutable address slices. The modes reuse the MessageIds text
engine through private purposes; public MessageIds behavior and work charges
are unchanged. Return Yield, Scalar or Complete, with an encoding diagnostic
that is final only after Complete. Scalars remain provisional response text;
a later resource failure retires the result. No event grants SMTP authority.

Parsed validates the entire bare addr-spec before emitting any scalar, then
replays only its raw parts under the live meter. Remove grammatical CFWS,
unfold quoted/literal folds and decode UTF-8, replacing noncharacters with
U+FFFD and recording a diagnostic. Preserve quotes, quoted pairs, domain
brackets, case and literal encoded-word-looking text. Invalid syntax emits
no text; excessive nesting and work retain their resource meaning.

Fallback bypasses address grammar, trims ASCII SP/HTAB/CR/LF at both raw
edges with charged visits, then unfolds and UTF-8-decodes one whole extent.
Malformed UTF-8 uses maximal-subpart replacement and sets the diagnostic;
noncharacters also become U+FFFD. An empty trimmed input completes without
any scalar or synthetic address. This independently enforces the same trim
used by group assembly, so that caller need not supply already-trimmed data.
Internal nonfolding CR/LF, NUL and other literal controls remain data. Both
modes preserve literal controls; later JSON must escape them, and outgoing
SMTP must apply its separate stricter validator. No NFC, case folding,
unquoting or encoded-word decoding changes the email address in either mode.

The non-Copy facade fits 416 bytes with shared syntax/replay progress,
fixed unfolding/charset state, one conversion byte and an error latch. It
uses no new arena or copied address. Each poll visits at most 256 source
bytes and charges at most 33 records/four output bytes. Charge both raw
conversion and decoded-byte visits, intermediate unfolding bytes and final
UTF-8 scalar widths. Parsed a@b costs 24 visits/56 records/six output bytes;
Fallback a@b costs eight visits/12 records/six output bytes. All failures
latch across fresh meters; cached Complete is inert. Tests and allocation
intervals cover spelling, folds, malformed UTF-8, controls, noncharacters,
empty/long values, validation-before-output and all work dimensions.
Display-name decoding/NFC, JSON serialization, response storage and complete
worker qualification remain open.

### 1.43 Validated phrase replay

M06ak adds a consuming `header_phrase::Cursor::into_validated` transition
that succeeds only after whole-phrase Complete. An incomplete parser returns
InvalidState; a failed parser retains its error. The opaque Copy `Validated`
proof holds only the immutable source slice, with no public constructor.
It proves phrase grammar, including UTF-8 and bounded comment nesting; it
does not authorize encoded-word interpretation or response publication.

`Validated::replay` creates a Copy `header_phrase::replay::Cursor` with
source, scalar offsets, one lexical phase, nesting/escape state and a sticky
error. Return the same raw Token events and trailing CFWS extent as the
validator; Yield cadence and work charges differ. Traverse prevalidated
comments/quotes bytewise, using the same atext predicate as validation.
Keep quotes, pairs, folds and exact gaps in their original extents. Copying
retains progress, including errors, but no work meter or output authority.
Every resumed or repeated traversal spends the caller's live meter again.

The proof fits 16 bytes and replay state fits 80 bytes. Each poll charges at
most 32 records and 32 source-byte visits; charge peeks and EOF records before
inspection. No output bytes are charged because events contain only offsets.
Replaying `a` costs two visits/four records, `""` two/three, and `a.` four/five,
in addition to validation. All work errors latch across fresh meters and
copies; cached Complete is inert. Earlier events remain provisional until
replay Complete because a later work refusal can retire the traversal.

Tests compare validation/replay events and exact extents over grammar
products, nested comments, quoted pairs, folds, Unicode and long input.
Checkpoint tests compare every resumed suffix and charge repeated traversal;
phase-specific work/deadline refusal and the allocation probe cover copies.
Display-name decoding, external delimiter placement, NFC source integration,
JSON publication and composed worker qualification remain open. A proof does
not supply comma, angle or group-colon context outside its slice.

### 1.44 Phrase display-name scalars

M06al adds `header_phrase::decode::Cursor` from a completed phrase proof,
its admitted whole field value and the phrase's exact extent in that field.
Construction requires pointer/length identity between that extent and the
proof's borrowed slice. Retain the whole field so placement checks inspect
actual neighboring bytes, including commas, angle brackets and group colons;
the decoder has no separate permission flag. This binding proves containment
only. The caller must supply the whole admitted field value: a narrower slice
would invent field boundaries and change placement. Field/form authorization
and selection of the complete phrase remain with that caller.

Classify whether the phrase consists solely of one quoted word, then replay
raw tokens. Only whole Atom tokens with valid immediate LWS/field-boundary
placement may use the existing encoded-word recognizer in Phrase context.
Quoted words and obsolete dots remain literal. Reject invalid placement by
retaining the literal spelling, even where a tolerant reader might recover.
Omit comments and collapse nonempty grammatical CFWS between words to one SP.
Suppress a pure LWS gap only between two recognized encoded words. Comments
never count as the immediate LWS required beside an encoded word; a gap
containing comments is not suppressed. Leading/trailing CFWS is omitted.

Remove quote delimiters and quoted-pair escapes, then unfold logical bytes.
For a sole quoted word, a charged prepass finds the first/last non-SP/HTAB,
non-NUL logical byte before emitting scalars, trimming its edges without
buffering whitespace. Mixed-word phrases preserve whitespace inside each
quoted word. Literal NUL is dropped; other literal controls remain.
Noncharacters become U+FFFD with an encoding diagnostic. Encoded controls
are dropped and malformed encoded text is repaired by the existing word
decoder. Decoded word spaces are preserved. NFC, comment fallback names and
JSON serialization remain separate. Scalars and diagnostics are provisional
until Complete.

Copy state fits 224 bytes, including a checked turn ordinal reserved for future
NFC checkpoint identity. Replay and word decoding occupy mutually exclusive
enum variants; a private source-bound token checkpoint recreates replay only
at a proven token boundary. Literal UTF-8 conversion uses a fixed four-byte
stack array. No token/name buffer or meter is retained. The private charging
seam preserves aggregate interpretation refusal for future NFC composition.
Each poll charges at most 230 byte visits and 227 records; scalar emission
here spends no output bytes, so the owner must debit serialized output.
Classification, trim prepasses, byte lookahead, conversion and copied replay
all spend the same live meter. Plain `a` costs six visits/17 records; `""`
costs four/14 and `" a "` costs 15/24, excluding initial grammar validation.
Errors latch across copies/fresh meters; cached Complete is inert.

Tests cover field-bound placement, quote/fold/trim policy, comments, encoding
repair, long Unicode, exact work, every copied suffix, refusal after output
and aggregate refusal inside replay. Allocation intervals cover composition
and refusal. Complete NFC Source and worker resource qualification remain open.

### 1.45 Normalized phrase display names

M06am adds `nfc::Cursor::from_phrase(proof, field, extent, scratch, meter,
header_budget)`. It binds the completed phrase proof to the field range as in
section 1.44, then composes that decoder with the existing NFC engine.
Construction is fallible for a mismatched source/range. The caller authorizes
the header form and supplies an admitted immutable field, excluding its final
line ending. Initial grammar validation remains separately charged by its
caller; complete header-form aggregate admission and response publication
remain outside this adapter.

The private Source now supports valid UTF-8, unstructured text and phrase
text. Phrase checkpoint identity uses the whole field's pointer/length,
phrase extent and checked successful-turn ordinal, plus pending canonical
decomposition. No source prefix is scanned to compare checkpoints. Copies
retain decoder/replay/trim state, including positions inside encoded words;
meters, aggregate budget, scratch and record credit remain borrowed once.
Diagnostics accumulate across original scanning and normalization replay.

The same 3072-byte Scratch and at most 1024 bytes for cursor plus aggregate
budget remain sufficient; each Source fits 256 bytes. Phrase polls execute
one normalizer transition and charge at most 231 aggregate steps/15 job
records, within the existing 256-step ceiling. All classification, trimming,
lookahead, conversion and replay visits pass through the live HeaderBudget
before work. Output is charged separately by `charge_output`; failure there
retires even a completed cursor. Cached completion is otherwise inert.

Tests cover literal/quoted/cross-word canonical and Hangul composition,
filtering before NFC, overflow with multiple combining classes, restoration
inside encoded words and pending canonical decomposition, and precise prefix
visits. A maximal one-MiB ASCII phrase fits the default foreground and header
budgets with 4194306 decoding visits, excluding initial grammar validation.
An added 10000-byte prefix costs 40000 visits even when the tail replays.
Shared aggregate exhaustion, progressed deadline refusal and output refusal
remain terminal. Allocation intervals cover the complete phrase/NFC fast and
replay paths. Comment fallback names, field assembly, JSON serialization and
complete worker memory/RSS qualification remain open.

### 1.46 Fallback comment display names

M06an adds `header_comment::Cursor`, an exact one-comment grammar validator.
The selected range includes its outer parentheses and no outer CFWS. It reuses
the bounded CFWS parser, requires a single comment spanning the entire slice
followed by successful completion, and consumes that success into a
private-field Copy `Validated` proof. A malformed or unfinished cursor cannot
produce a proof. Nesting/work refusals stay terminal. Mailbox selection and
field/form authorization remain the caller's responsibility; a raw
`header_mailbox::Name::Comment` range alone is not proof.

`Validated::decode()` produces a Copy scalar cursor. It drops the outer
parentheses, retains nested parentheses as display text, removes quoted-pair
backslashes, and unfolds after unquoting. POLICY.md specifies grammatical
whitespace trimming/collapse and RFC 2047 Comment placement. Recognition
examines original unescaped bytes only. Both edges of nested comments and
whole quoted-pair tokens are grammatical boundaries; a word may directly
precede or follow them. A quoted pair's escaped byte cannot start a word.
Unquoting or decoding never manufactures syntax. Adjacent plain ctext or
another encoded word requires LWS. Only a pure LWS gap between recognized
words is suppressed. A pending separator emits only when another scalar
survives filtering, so dropped NUL/encoded controls cannot leave a trailing
separator. Escaped or encoded spaces remain data.

Literal NUL is removed, other literal controls are retained, noncharacters
become U+FFFD and set the encoding diagnostic. The shared word decoder removes
encoded controls, repairs encoding faults and preserves decoded spaces. Owner
serialization charges output and escapes JSON controls.

`nfc::Cursor::from_comment(proof, scratch, meter, header_budget)` composes
this source with the existing resident normalizer. Each Copy checkpoint
compares immutable source pointer/length and a checked successful-turn
ordinal, plus pending canonical decomposition. Copies contain no meter or
scratch credit; all validation is caller-charged and subsequent
lookahead/conversion/replay spends the live aggregate budget. Diagnostics
accumulate through replay.

The validator fits 96 bytes, the proof 16 and the scalar cursor 192. Validator
polls use at most 160 visits/33 records; scalar polls at most 225 visits/227
records, without output charges. Empty `()` decoding costs zero visits/two
records; `(a)` costs four/three, excluding validation. Errors latch across
copies/fresh meters and cached completion is inert. NFC retains the 256-byte
Source, 3072-byte Scratch and 1024-byte cursor-plus-budget ceilings. Its
comment polls fit 231 aggregate steps and 15 job records. These are upper
bounds, not a claim that every poll attains them.

Tests cover exact proof rejection, nested/escaped text, actual encoded-word
placement, whitespace/filtering order, every copied suffix, terminal refusal,
canonical/Hangul NFC, hostile multi-class replay and prefix accounting.
Allocation intervals cover validation plus decoding/NFC and refusal. Complete
header-form aggregate admission, field assembly, JMAP publication and worker
stack/RSS qualification remain open.

### 1.47 JSON output from normalized scalar sources

M06ao adds `json_string::Cursor::new(&mut nfc_cursor)`. It exclusively borrows
an already constructed NFC cursor and retains at most six escaped bytes; no
full string is copied. Field/form authorization, source selection, and
semantic scalar filtering stay with the selected source's owner. This adapter
serializes exactly one quoted JSON string. It is not a complete header-form or
JMAP response publisher. The source must be unpolled when wrapped. Dropping
the adapter before Complete abandons the whole property, including any emitted
prefix and staged bytes; do not rewrap that advanced source. A retry
constructs a fresh source from the original input and spends the remaining
live work budget without refunding earlier charges.

`poll(tick, output)` returns a written-byte count and Yield, NeedOutput or
Complete. One turn either stages one opening/closing quote, polls the source
once, or drains at most six pending bytes. Even a one-byte output slice
progresses. An empty slice returns NeedOutput without advancing or charging,
but checks the live deadline. Bytes beyond the reported written prefix remain
untouched. Complete is returned only after the closing quote is copied.

Escape quote/backslash and U+0000..U+001F per RFC 8259 section 7. Use short
escapes for backspace, form feed, LF, CR and HTAB, and lowercase hexadecimal
`\u00xx` for other controls. Other scalars retain UTF-8, including solidus,
supplementary characters and U+2028/U+2029. This is JSON output, not HTML or
JavaScript embedding; no embedding-specific escaping is promised. Scalar
filtering and encoding diagnostics come from the chosen NFC source. I-JSON
character compliance is likewise the source owner's responsibility (RFC 7493
section 2.1): the decoded header/name constructors repair noncharacters, but
plain `nfc::Cursor::new(&str)` preserves them and cannot by itself qualify
JMAP output.

Precharge each entire escaped scalar or quote to that source's same live
output meter before copying any of it. Fragmenting output never charges a
pending byte again. Normalization source visits/steps keep their existing
aggregate accounting. Check the deadline on active entry, including pending
output drains. `check_deadline(tick)` provides an explicit post-turn/final
check and retires even a completed cursor on refusal. Cached completion is
otherwise inert. Any refusal is sticky and invalidates the entire provisional
property, including earlier output. The containing response owner must retain
those bytes until its own complete-field checks succeed.

The adapter fits 64 bytes of framing state and borrows NFC's separate 4 KiB
reservation. Six pending bytes and four local UTF-8 bytes bound escaping. A
turn does one NFC poll or one fixed output action, with no heap growth. Tests
pin literal JSON, all Unicode scalar encodings, every output width from one
through eight, hostile NFC replay, source diagnostics, output charges, and
empty/output/deadline refusal. Allocation intervals cover normalization plus
one-byte JSON drains and sticky refusal. Aggregate field admission, retained
response ownership and complete worker qualification remain open.

Source: [RFC 8259 sections 7 and 8.1](https://www.rfc-editor.org/rfc/rfc8259.html#section-7).

### 1.48 JSON output preserving Raw and address text

M06ap adds `json_string::Cursor::from_raw(&mut raw_cursor, &mut meter)` and
`from_address(&mut address_cursor, &mut meter)`. These select the existing Raw
and parsed/fallback address projections directly, with no normalization or
encoded-word interpretation added by the serializer. Supply an unpolled source
and its job's live meter. Partial abandonment invalidates the whole property
and any staged/output bytes; retry with a fresh source from the original input
without refunding spent work. Authorization, source binding and whole-field
admission remain with the enclosing owner.

The private source enum holds exclusive borrowed references, with exactly one
active source and no owned parser or copied meter. The normalized constructor
keeps its existing API. Raw/address constructors share the same six-byte
staging, bounded drains, live deadline checks, output precharging and sticky
retirement. The complete JSON wrapper still fits 64 bytes. The Raw cursor
remains in the 2 KiB decoder/HTML/snippet state of the 32 KiB conversion
region; the address facade remains in the 16 KiB parser reservation. Neither
path borrows NFC scratch.

Raw keeps source folds, case, decomposed characters and literal encoded words,
while applying its existing NUL/UTF-8/noncharacter policy. Address projection
keeps address identity, removes grammatical CFWS or trims/unfolds fallback
according to its selected mode, and applies its existing repair policy. JSON
only escapes the resulting scalars. Raw and address decoders already replace
noncharacters; encoding diagnostics remain source-owned. No JSON completion
proves that a recovered address can be used for SMTP.

Address conversion already charges intermediate unfolding and scalar bytes.
The serializer additionally charges escaped JSON bytes and quotes once; it
neither refunds earlier conversion nor double-charges fragmented JSON drains.
Raw charges its original decode work plus exact serialized output. Raw/Address
variants identify source errors; direct serializer charges use Work only for
those borrowed-meter modes. The normalized mode retains Source for both NFC
and serializer refusals, so it does not distinguish those origins. Every
variant retires the whole provisional string, including an opening quote
emitted before malformed address validation finishes.

Tests cover output widths one through eight, preserved combining sequences,
folds, quoting, controls, case and encoded-looking addresses; repair
diagnostics; and exact conversion-plus-JSON charges. Source, output and
final-deadline refusals remain terminal. The allocation probe covers Raw,
parsed/fallback addresses and malformed refusal in both registered modes. This
supplies string components; list/group assembly, complete aggregate admission,
retained response publication and worker resource qualification remain open.

A direct serializer refusal stops the wrapper and its borrowed meter. It need
not latch into a Raw/address source that was not polled by that refusal. The
owner must retire that progressed source with the property and must not attach
a fresh meter or rewrap a copy. This differs from normalized output charging,
which also retires the NFC source. Source-reported failures retain each
source's existing latch.

### 1.49 Shared JSON framing and private mail adapters

The crate-private `Frame` and `ScalarSource` seam adapts the shared
`td_json::string::Frame<json_string::Error>` introduced by M06bk. The shared
crate owns framing, escaping and refusal latching; mail adapters own scalar
decoding, NFC, clock admission and error mapping. The existing source adapter
is also crate-visible, so coordinators reuse its error/status mapping. Sources
must bound each poll and check the live deadline even for zero-byte output
charges. Frame owns quote/source/closing phase, six pending bytes, offsets and
the sticky failure latch, within 32 bytes. It retains no source, meter,
scratch or replay credit. One `poll` takes a short mutable source borrow and
performs the same bounded staging/source/drain action. The source supplies
scalar polling and live output charging; semantic filtering and diagnostics
stay with the selected projection.

A containing coordinator must bind one unpolled source and its original job
meter for the frame's whole lifetime, retain them across every short borrow,
and abandon the complete property on drop/refusal. Neither seam checks source
identity or authorizes a replacement source or meter. The public
`json_string::Cursor` keeps its existing exclusive source borrow and delegates
to Frame, with unchanged constructors, errors, diagnostics, output
precharging, deadlines and 64-byte ceiling.

An owner may now keep a Raw/address parser and Frame by value, borrowing its
disjoint fields only during each poll, without self-references or allocation.
This does not make NFC own the scratch, meter or budget that it borrows; those
remain externally owned under its existing 4 KiB contract. Complete field
admission and retained response publication are still separate work.

Existing source-mode, fragmentation, exact-charge and failure fixtures run
through the extracted frame. A movable Raw-owner fixture exercises repeated
short borrows, output parity, exact live counters and refusal retirement. A
non-latching fault source independently pins Frame refusal, untouched sinks
and inert cached completion. Existing allocation intervals cover the
delegating public paths. This is framing reuse, not a complete field
coordinator or worker qualification.

### 1.50 Aggregate header selection admission

M06ar adds `header_select::Cursor::poll_with_budget` with one live job meter
and the existing email `nfc::HeaderBudget`. An admitted owner uses this entry
point from the first poll and never interleaves ordinary `poll`, replaces its
meter or renews the email budget. A mode latch rejects interleaving with
`InvalidState`; stable meter/budget identity remains the owner's obligation.
Share the same budget across field selection, repeated requested properties
and subsequent normalized projections. Request property-key parsing remains
job-metered separately; it does not inspect mail. The existing plain
selector/scanner APIs retain their job charging for callers that have not yet
composed aggregate admission.

A private scan work adapter propagates typed budget refusal through the raw
scanner and selector. Before each scanner byte lookup, charge one step and one
visit if a byte is present. Repeated lookahead charges again. EOF/NeedInput
transitions cost a step without a byte; field emission costs another step.
Field-name comparisons visit both operands in chunks of at most 32 pairs and
charge one step per pair before comparing every pair, including after a
mismatch. Length-only rejection and each final Match/Complete event cost one
step. Name comparisons also debit the request-side operand as bounded work.
Body bytes beyond the scanner's required boundary lookahead are not visited.

The aggregate budget debits those exact visits and steps. Its existing 16-step
job-record precharge is retained: private non-copyable selector credit starts
at zero, survives yields and is never refunded or restored by replay. Field
events are included in step accounting. Aggregate scanning admits at most 255
lookups per turn, reserving one step for emission to retain the 256-step
fairness ceiling. Ordinary scanning retains its 256 lookups. Ordinary non-
aggregate callers retain their original per-event job records; aggregate calls
use the step precharge instead. At most 255 source visits, 256 steps and 16
job records are charged per selector poll. Output bytes and unlink counters
are untouched. The selector still fits 384 bytes and its scanner 128 bytes; no
buffer, heap allocation or new worker reservation is introduced.

An aggregate refusal returns `InterpretationLimit` and retires that budget for
later selections and NFC cursors. Job refusal remains `Work(Stop)`; invalid
private accounting is `InvalidState`. Refused charges change no remaining
counters and no corresponding source/comparison state. Earlier successful work
in the same turn stays charged; all prior matches remain provisional and must
be discarded. The selector latches failures even if a caller later supplies a
different meter. Cached completion is inert; its owner still performs the
existing final clock/cancellation checks before publication.

Tests pin exact small-field accounting, long-field turn bounds, legacy parity,
scanner/comparison precharge refusal, repeated-property exhaustion and refusal
shared with normalization. The allocation probe repeats long field names until
the real aggregate cap refuses. Other form grammar/conversion stages still
need aggregate integration; this increment does not enable JMAP publication or
claim complete worker memory qualification.

### 1.51 Aggregate Raw conversion and JSON output

M06as adds `header_raw::Budgeted`, which owns a fresh Raw cursor and private
step credit while exclusively borrowing the job meter and email HeaderBudget.
It is neither Copy nor Clone; ordinary copied Raw cursors remain available
under the original job-only API. Aggregate copies cannot duplicate credit or
renew budgets. Its scalar output preserves the existing Raw policy: no NFC, no
encoded-word interpretation, NUL removal after UTF-8 decoding, replacement of
invalid maximal subparts and noncharacters, and literal folds/controls. Raw
now owns its typed Error with Work, InterpretationLimit and InvalidState.

Before each active poll, charge one owner transition. The charset work adapter
charges one step per byte visit and scalar emission, and one for zero-byte EOF
work. Lookahead is precharged before loading the source byte; invalid UTF-8
lookahead is charged again when revisited. At most four visits, six aggregate
steps and one prepaid job record occur in a poll. N ASCII bytes take N visits
and 3N+2 steps including completion. Source-level replay cannot copy this
wrapper's budgets or credit. Earlier successful charges survive later refusal.

The wrapper fits 96 bytes, including the existing Raw state and references to
the existing email/job budgets, inside the 2 KiB decoder/HTML/snippet
reservation. It needs no NFC scratch or heap backing. Error latching covers
parent admission, child decode, output and deadline refusal. Aggregate
exhaustion is shared with later owners; job refusal retains Work(Stop). Cached
scalar completion is inert.

`json_string::Cursor::from_budgeted_raw` borrows an unpolled wrapper. The same
private Frame and 64-byte public serializer ceiling apply. Output staging
checks the live email/job state and charges escaped bytes/quotes once before
copying; zero-byte checks do not consume aggregate steps or credit. Both
source and serializer errors use Raw, including output/deadline refusal; the
wrapper also latches those errors. Thus final serializer deadline refusal
retires even a completed source. Every output byte remains provisional until
the containing property succeeds. Dropping the serializer early abandons that
property and source; rewrapping progressed state is not allowed.

Tests cover literal identity, repair, bounded and exact work, aggregate/job
refusals, non-copyable ownership and fragmented JSON. An allocation interval
covers long one-byte JSON output and output refusal in both registered modes.
Selection-to-value ownership, other forms and retained publication remain
follow-on work; this component does not qualify complete worker memory.

### 1.52 Complete provisional Raw header values

M06at adds `header_value::Raw`, composing occurrence selection, aggregate Raw
conversion and JSON framing for one already parsed Raw property. Following RFC
8621 section 4.1.3, a single occurrence returns the last matching field or
null; `:all` returns an array in source order, including an empty array when
absent. An empty field remains an empty string. Non-Raw properties return
UnsupportedForm before work or output. The caller authorizes and retains the
complete resident header source, property key and supplied header-byte limit.
Base offsets and Prefix/EOF meaning remain those of the selector.

The coordinator exclusively borrows one job meter and email HeaderBudget for
its whole lifetime. Its ownership enum moves those same references into each
fresh budgeted Raw source and accepts them back only after successful scalar
completion. The private consuming handoff rejects unpolled, partial or failed
sources. No cloned meter, copied credit, self-reference or refund is involved;
unused per-value prepaid credit is discarded. Selector credit survives value
conversion and remains attached to the same job. Field extents are checked
against the original resident slice before conversion.

A poll performs at most one selector or Raw/JSON child turn, plus bounded
fixed ownership and literal actions. Literal punctuation and null use four
fixed bytes; string staging remains the existing six-byte Frame. Every output
byte, including brackets and commas, is precharged once before fragmentable
copying. Output chunks contain at most six bytes. Empty output checks the live
budgets/deadline and returns NeedOutput without advancing. JSON framing costs
no header-interpretation steps; child bounds remain at most 255 visits, 256
steps and 16 job records per coordinator turn. The complete coordinator fits
640 bytes in the existing 16 KiB parser-state reservation; source/property
arenas and the caller's existing JSON output buffer are separate.

All output is provisional, including values emitted before a later header scan
fails. The caller writes chunks only to ADMISSION.md section 4's unpublished
response tail. A field Complete is insufficient to publish a method index or
HTTP response; the whole method/request must satisfy that retention contract.
Any refusal discards its method's provisional tail. No whole-value buffer,
response-sized heap object or truncation-to-success is introduced. Spool I/O
and response publication are not implemented here.

Success returns the selector's exact header/body boundary and final encoding
diagnostic. Cached completion is inert. Explicit final deadline checks remain
live and invalidate a completed value on refusal. Failures latch with typed
selection, Raw, JSON, job or aggregate causes. Direct framing charges return
Work; value-owner checks/comma staging return Raw(Work); string-frame charges
return Json(Raw(Work)); selector work returns Selection(Work). Aggregate
causes retain the analogous nesting. Callers classify the underlying cause,
not only the outer wrapper. Failures leave caller output untouched on later
calls. Tests cover last/all/absent/empty/repaired fields, base offsets, actual
EOF and incomplete prefixes, one-byte output, exact charges across values, late
scan refusal, output limits, ownership handoff and final retirement. An
allocation interval covers long values and failure after a provisional prefix.
Other parsed forms, spool retention and complete worker qualification remain
separate increments.

### 1.53 Complete provisional Text header values

M06au adds `header_value::Text` with an Input descriptor, the caller's NFC
Scratch and the same exclusive job/email budget borrows. Input holds resident
bytes, base offset, header limit, parsed property and Prefix/EOF meaning. Only
Text-form properties are admitted. The first active poll additionally admits
Subject, Comments and Content-Description, plus user-defined X- fields,
case-insensitively, before staging any JSON. M06bj extends the unstructured
path using RFC 2045 section 8 and RFC 2047 section 5's field rules.
Parentheses and quotes in these fields are text, not structured comments or
quoted strings. M06bm also admits Keywords and List-Id with original
phrase/comment placement as specified in POLICY.md. Quotes, quoted pairs,
comments and punctuation remain Text data, while List-Id identifier bytes
never admit words. M06bn admits Content-Type and Content-Disposition with
comment-only word placement, preserving parameter spelling as Text data.
RFC 2231 decoding and filename compatibility remain derived-metadata work.
Other MIME fields and unknown non-X- fields still return UnsupportedGrammar.
This is an implementation limit, not an invalid-property or invalidArguments
claim; Text-form authorization alone cannot authorize encoded words in
structured syntax. Last/all, absence, provisional output, backpressure and
final retirement follow section 1.52; Raw's public API and identity stay
unchanged.

A private generic core owns the shared selector, punctuation, JSON Frame and
failure state. Its two private projections provide Raw or normalized Text
sources; there is no trait object, heap allocation or duplicated framing state
machine. The Text source uses the existing NFC cursor with the selected
resident scalar grammar. It removes initial SP, unfolds while retaining
following whitespace, decodes originally permitted encoded words and
normalizes the filtered scalars. Keywords/List-Id and MIME parameter fields
add fixed lexical state; whole escaped UTF-8 characters, including repaired
malformed prefixes, must finish before a subsequent word can begin.
Quotes/comments remain unfolded
source bytes rather than display-name projection. Comment nesting above 32
refuses with an interpretation-limit error.
Grammar admission dispatches by name length and prepays each comparison.
Subject/Comments keep their seven/eight visits and one step. Content-
Description costs nineteen visits/one step. Seven-byte names try Subject,
then List-Id (fourteen visits/two steps); eight-byte names try Comments,
then Keywords (sixteen visits/two steps). Failed known-name comparisons
precede a separately charged two-byte X- prefix check.
Content-Type costs twelve visits/one step; a twelve-byte miss costs fourteen
visits/two steps including the prefix check.
Nineteen-byte names try Content-Description then Content-Disposition,
costing thirty-eight visits/two steps for the latter or forty visits/three
steps after a failed match and prefix check. Dispatch has dimensional maxima
of forty visits, three steps and three job records. Other name lengths check
only the prefix, with no scan of the remaining extension name. A name shorter
than two bytes has no byte comparison but still costs one step
and one job record. Use the original job/email budgets and discard unused
prepaid credit; empty output performs only the live deadline check until
capacity is supplied.

The same ownership enum moves scratch and budget borrows into each fresh NFC
cursor. A private consuming NFC handoff returns them only after successful
Done with no failure. Checkpoints and unused prepaid credit cannot escape or
be copied; the next field starts with fresh cursor state over the reused
scratch. Original job/email counters and selector credit persist. Text source
checks, comma staging and handoffs report Text(nfc::Error); its string
serializer reports Json(Source(nfc::Error)). Direct framing and selection
retain their existing error wrappers. Callers classify the underlying cause.

Raw still fits 640 bytes. The Text coordinator fits 1664 bytes including its
inline NFC cursor, within the existing 16 KiB parser reservation. Its
3072-byte scratch remains in the 4 KiB NFC region; the standalone
checkpoint-state slice of that region is unused for this path. No second NFC
cursor is retained. Each poll calls at most one child plus fixed
ownership/literal actions, with at most 255 visits, 256 interpretation steps,
16 job records and six copied output bytes. These component bounds do not
qualify combined worker stacks or native/process memory.

Tests distinguish Raw identity from Text decoding/NFC, cover last/all and
absence, repair diagnostics, encoded-word and fold handling, overflow segments
followed by a second field, exact charges against separate selector/converter
runs, Content-Description/X- admission, exact classification costs, partial
job/email admission refusals, unsupported-field refusal, Keywords/List-Id
and MIME parameter-field placement and escaped-character repair,
comma/output exhaustion, partial handoff, late scan refusal and final
deadline retirement.
The allocation probe covers overflow plus subsequent scratch reuse and late
failure in both registered modes. Other parsed forms and ADMISSION.md's
unpublished response-spool implementation remain separate.

### 1.54 Budgeted resident Date parsing

M06av adds `header_date::Budgeted`, owning one Date cursor and exclusive
borrows of the original job Meter and shared per-email HeaderBudget. It
cannot be cloned or copied. Polling returns the same Yield or
Complete(Option<Date>) outcomes as section 1.31, including whole-field
validation and malformed-date None. No formatted or JSON output is produced.

The existing Date and CFWS parsers use a private generic charging seam. Plain
public polling still uses the original Meter and retains its exact charges.
The budgeted adapter charges each original source-byte visit and record as an
interpretation step, with at least one step for an EOF-only charge. CFWS
lookahead, UTF-8 revalidation, token delimiters and the parent's trailing-FWS
visit are charged before source access. Sixteen steps prepay one job record;
credit stays private to this non-replayable wrapper across all child turns.
Dropping it discards unused credit without refunding or resetting counters.
Subsequent header projections retain the same job and email budgets.

InterpretationLimit remains distinct from Work(Stop), malformed-date None and
NestingLimit. CFWS callers carry the typed aggregate refusal through their
existing error adapters; their plain APIs do not acquire an aggregate budget.
A failure retires the wrapper permanently. Cached Complete is inert, while
check_deadline performs a live zero-cost check even after Complete and retires
that result on refusal. The enclosing owner must make that final check before
publishing any dependent result. It also brackets active polls with fresh
clock/cancellation checks as for the other resident parsers.

The wrapper fits 224 bytes, including inline Date/CFWS state, budget
references and credit, in the existing 16 KiB parser-state reservation. One
poll charges at most 161 source visits, 194 interpretation steps and 13 job
records, with zero output bytes. The same live budget covers nested comments
and dates; there is no child budget reset, copied string, heap allocation or
new arena. These are component bounds, not a combined worker-stack
qualification.

Tests compare plain and budgeted parsing results and visits, exercise long
Unicode comments, nesting and malformed tails, pin exact EOF/fold/revisit
charges, and prove refusal before source access. They cover sticky aggregate
and job exhaustion, credit discard between fields and live final checks.
Every partial byte/step budget is tested across malformed UTF-8, folds,
malformed trailing comments and valid dates with comments. The
allocation probe adds long valid dates, malformed tails, budget reuse and
terminal deadline refusal in both registered modes. Sections 1.55 and 1.56
add aggregate formatting and complete provisional Date property JSON.

### 1.55 Budgeted Date formatting

M06aw adds `header_date::project::render_with_budget` over copied Date
components, caller output, a tick and the original job/email budgets. It
shares the existing formatter through a private charging seam; plain render
retains its original Meter charges and behavior. All outcomes from section
1.32 remain distinct: Date, OutOfRange and LeapSecondUnverified. Aggregate
exhaustion returns InterpretationLimit, not a null-producing parse outcome.

The bounded component/calendar/formatting pass costs eight interpretation
steps; qualifying a valid second-60 component adds six steps for the existing
pinned-table path. Those fixed charges precede their work. One invocation
prepays at most one 16-step job record and discards unused credit on return.
Source-byte counters are untouched. Actual formatted output is still charged
exactly once, before any caller-buffer write: 20 bytes for UTC Z or 25 bytes
for unknown -00:00. Invalid or unqualified dates produce no output charge.
Capacity and work errors also leave the output untouched, although already
performed interpretation remains charged. Repeated calls retain the same
budgets; the aggregate exhaustion latch and job stop remain authoritative. The
private adapter rejects mixed interpretation/output charges before any debit;
the formatter uses separate work and output admission points.

The private work adapter fits 24 bytes of bounded transient state. It adds no
arena, source copy or owned string; the 25-byte caller output has the same
placement/lifetime obligation as the plain formatter. This is one bounded
formatting call, not a complete Date property or a combined stack/RSS claim.
Output remains provisional; the owner performs the final live deadline check
before publishing it and retires all dependent values on enclosing failure.

Tests compare plain and budgeted results, preserve known/unknown offsets and
pinned/unverified leap outcomes, pin exact step/record/output costs, and cover
aggregate, record, output, capacity and deadline refusals before mutation.
Repeated projection proves credit discard without budget reset. Allocation
intervals cover successful dates/leaps, invalid/unverified outcomes and output
and deadline refusal in both registered modes. Section 1.56 adds Date property
JSON; unpublished response-spool retention remains follow-on work.

### 1.56 Complete provisional Date header values

M06ax adds `header_value::Date` over the resident Input descriptor and the
original job/email budgets. Only Date-form properties are admitted. It uses
the same selector, last/all/absence framing and provisional-output contract as
Raw and Text; missing values are null or [], and malformed fields become null
values in their selected positions. It does not publish a method result or an
HTTP response. Late selection/resource failure retires all output.

The private Date projection first validates the whole field with Budgeted Date
parsing, then consumes that successful cursor to recover its original budget
borrows. A private formatter call retains the budgets and renders into 25
bytes inside a 27-byte inline staging array. Known/unknown timestamps are
quoted directly: checked Date formatting emits only fixed ASCII syntax, so no
escaping or second scalar parser is required. Malformed or out-of-range dates
stage null. LeapSecondUnverified also stages null and retains a separate flag.
`has_unverified_leap` is final only after property Complete and ORs that
diagnostic across selected values; it does not describe unselected fields. Raw
bytes and the leap table remain unchanged.

Date parsing and formatting charge their aggregate work as in sections 1.54
and 1.55. Formatting prepays the 20/25 timestamp bytes; staging then prepays
the two quotes, or all four null bytes. Array punctuation uses the shared
core. Every external byte is charged once, before staging/draining it; drains
copy at most six already-paid bytes and never charge again. An output refusal
can leave earlier internal formatting work charged while exposing no bytes
from that timestamp. Previously emitted array bytes remain provisional.

Source parsing and checks while that parser owns the budgets report Date;
formatting reports DateProjection; later literal/quote checks report Work or
InterpretationLimit. In particular comma staging on a fresh parser uses
Date(Work). Selection retains its existing wrappers. Callers classify the
underlying resource cause; none becomes a null-producing malformed outcome.
Date(NestingLimit) also remains a typed resource failure, never null. Both
private owners reject consuming handoff before successful completion or after
failure. Empty-output polls and final deadline retirement follow the shared
core; cached completion alone does not perform final admission.

The Date coordinator fits 896 bytes in the existing 16 KiB parser region,
including one inline Date/CFWS cursor, its budget borrows, the 27-byte staging
array and shared selection/framing state. It replaces standalone Date state
and output storage on this path. It needs no NFC scratch or extra arena. The
transient formatter adapter remains at most 24 bytes. Poll ceilings stay 255
source visits, 256 aggregate steps, 16 job records and six externally copied
output bytes. Those component bounds do not qualify combined worker stacks or
native/process RSS.

Tests cover widths one through eight, last/all/absence, known/unknown offsets,
malformed/out-of-range dates, pinned/unverified leaps and diagnostic scope.
Composed charges match separate selection/parsing/formatting. Tests exhaust
partial aggregate step budgets, pin timestamp/comma output refusal, retire a
late scan failure, check final deadlines and reject partial/failed handoffs.
Allocation intervals include long Unicode comments, multiple dates, leap
outcomes, one-byte drains and late selection refusal in both registered modes.
Other structured forms and unpublished response-spool retention remain open.

### 1.57 Budgeted resident MessageIds grammar

M06ay adds `header_message_ids::Budgeted` for one resident field value. It
retains the original job Meter and per-email HeaderBudget exclusively, with
private non-copyable prepaid credit. Strict and ObsoletePhrases preserve the
plain parser's grammar and provisional Begin/Part/End events. Only Complete
validates the whole field; malformed tails, nesting refusal or resource
failure retire every prior event. The caller still authorizes ObsoletePhrases
only for References or In-Reply-To. This parser does not classify field names,
convert scalar output, or compose property JSON.

The shared private Parsing adapter charges one aggregate step per source
visit and grammar record, with at least one step per charge (including EOF).
It prepays one job record per sixteen steps, preserves byte revisits and
rejects output/unlink charges. Raw and Date use the same adapter with their
existing owner-level charges unchanged. The private error mapping is shared
with the other aggregate adapters; each retains its distinct charging policy.
Plain Meter entry points retain their exact previous charges and results.
Delimited-token aggregate failures propagate as InterpretationLimit through
MessageIds; mailbox code handles the same typed variant explicitly.

MessageIds child CFWS and delimited turns share the owner's original budgets
and credit. One poll charges at most 160 source visits, 192 aggregate steps,
12 job records and zero output bytes. Empty strict input costs zero visits
and three steps before Malformed; the obsolete empty list has those same
costs and completes. The list `<a@b>` costs fourteen visits and 31 steps.
Unused credit is discarded with the field. Earlier successful charges remain
when a later operation refuses. Aggregate exhaustion latches on the email
budget; a job stop retains its own typed cause without exhausting that email
budget. Refused operations do not consume source bytes.

Cached Complete is inert. `check_deadline` performs live admission even after
completion and latches failure, so callers must use it before retaining the
result. The wrapper fits 288 bytes including its inline grammar/CFWS/delimited
state and budget references, within the existing 16 KiB parser reservation.
It replaces the standalone grammar cursor on this path and adds no arena,
owned field copy or conversion scratch. The transient shared adapter fits
three references. These component bounds do not qualify combined workers or
native/process RSS.

Tests compare plain and budgeted events, outcomes and visits across strict
and obsolete modes, long Unicode input, folds, malformed tokens and nesting.
They exhaust partial byte/step budgets, including refusal after End, and pin
pre-access refusal, field-credit retirement, job limits and final deadlines.
An allocation interval covers long comments/atoms/delimited tokens, repeated
fields, malformed tails and terminal deadline refusal in both registered
probe modes. Section 1.58 adds aggregate conversion; complete MessageIds
property JSON and unpublished response-spool retention remain follow-on work.

### 1.58 Budgeted validated MessageIds text

M06az adds `header_message_ids::project::Budgeted`, retaining the original
job/email budget borrows and private prepaid credit across full validation,
replay, unfolding and UTF-8 conversion. It accepts the same resident field
slice and explicitly authorized mode as section 1.57. Whole-field malformed
or nesting refusal precedes any Begin/Scalar/End event. Later resource failure
still retires all provisional text. The encoding-problem diagnostic is final
only after Complete; cached completion is inert and explicit final deadline
admission can retire it.

Grammar, source revisits, parent records and UTF-8 consumption use the shared
Parsing policy. Each unfolding transition charges a step even for EOF or
backpressure. Output-only operations first charge one aggregate step, then
prepay job output bytes. A subsequent output refusal retains that admitted
interpretation step and any prepaid job record. Mixed output/input/record or
unlink charges are invalid before counters change. Plain APIs retain their
previous byte/record/output charges and turn sizes.

The private conversion path caps unfolding at 127 transitions per child turn;
the public unfolder retains 256. This leaves room for the parent's step and
both transition and byte charges, keeping a conversion poll within 160 source
or intermediate visits, 255 aggregate steps, sixteen job records and four
output bytes. Parsing/replay and byte conversion share one field's credit;
unused credit is discarded between fields. An empty obsolete list costs zero
visits, ten steps and one job record. Output work still includes intermediate
unfolding bytes and final scalar UTF-8 lengths. JSON framing and escaping are
separate work; none is emitted by this helper.

The unfolder now exposes typed Error variants Work, InterpretationLimit and
InvalidState, with a private generic work entry point. Its public Meter call
cannot produce InterpretationLimit. Both the unfolder and enclosing cursor
latch failures; internal copied decoding state never carries a budget or
credit. Byte lookahead is charged before reading input, including revisits
after nonfold endings. The public failure accessor retains the typed cause.

The budgeted converter fits 416 bytes, including inline parser, unfolding and
charset state, one conversion byte and original budget references. It replaces
the standalone converter in the existing 16 KiB parser reservation, without
NFC scratch, retained strings, lists or another arena. The transient adapter
uses three references. Combined worker stacks and native/process RSS remain
unqualified.

Tests compare plain/budgeted events, diagnostics, visits and output charges;
cover folds, controls, decomposed text, noncharacters and long input; exhaust
partial aggregate budgets through validation and replay; and pin output
refusal, retained costs, field-credit retirement and final deadlines. Probe
intervals exercise long Unicode values, folded tokens, noncharacter repair,
malformed tails and terminal refusal in both registered modes. Section 1.59
adds MessageIds property JSON; unpublished response retention remains open.

### 1.59 Complete provisional MessageIds header values

M06ba adds `header_value::MessageIds` over the resident Input and original
job/email budgets. Each selected field becomes an array of identifier strings
or null; `:all` adds an outer array in wire order. Missing last/all values are
null/[] respectively. Mode classification is part of the first active turn:
only case-insensitive References and In-Reply-To permit obsolete phrases.
Length selects at most one candidate; its at most eleven compared bytes and
one aggregate step are charged before comparison. Other names use Strict.
The shared projection validation hook retains that mode in its workspace
across selected values; no grammar classification occurs during construction.

The converter validates the whole field before emitting its first Begin, so
the value cannot expose an array or quoted prefix from malformed syntax. A
malformed validation outcome consumes the failed converter through a private
checked handoff and stages null using the original budgets. This handoff
accepts only Malformed during Validate; partial state, successful state,
nesting or resource refusal cannot use it. The ordinary consuming handoff
requires successful Complete. Both discard field-local prepaid credit.
Nesting and resource errors remain errors and retire all provisional output.

Each Begin opens one existing JSON string frame. Scalar events stream through
that frame, and End closes it; quotes, backslashes and controls are escaped,
while decomposed identifiers retain their identity and noncharacters use the
converter's replacement diagnostic. Empty obsolete lists stage []. Between
items, array commas and closing brackets use four inline staging bytes. A
frame remains bound to one converter and one identifier until it completes;
its parent cannot advance to the next item during staged-byte drainage.

Conversion keeps its existing intermediate/scalar output charges. JSON
strings and array/null literals charge every additional external byte once
before staging; drains copy at most six prepaid bytes without recharging.
The composed job/email totals match independent selection and conversion,
plus name classification and exact JSON output. Source and literal work can
report MessageIds errors; framed source/output errors retain Json(MessageIds).
After malformed handoff, null staging uses Work/InterpretationLimit directly.
Callers classify the underlying cause rather than one phase's wrapper.

Empty-output polls perform only live admission; cached completion is inert.
Final explicit admission can still retire the whole property. The encoding
problem flag is final at property Complete and describes selected fields.
The composer grants no method or HTTP publication authority: previously
emitted values and outer-array prefixes remain an unpublished response tail.

The coordinator fits 1024 bytes in the existing 16 KiB parser reservation,
including its converter, budget references, selector, shared JSON frame and
literal buffers. It replaces the standalone converter on this path and uses
no NFC scratch, retained identifier string/list or arena. Turn ceilings remain
255 source/intermediate visits, 256 aggregate steps, sixteen job records and
six external output bytes. A scalar turn also charges its converted UTF-8
bytes, for at most eight combined conversion/JSON output bytes per poll.
Combined worker/native/RSS qualification is open.

Tests cover widths one through eight, last/all/absence, authorized modes,
null versus empty arrays, folds/escaping/identity, diagnostic scope, exact
composed charges, output/aggregate cutoffs, late selection refusal and checked
handoffs. Allocation intervals cover long values, malformed fields, repairs,
one-byte drains and late refusal in both registered modes. Other structured
forms and unpublished response-spool retention remain follow-on work.

### 1.60 Budgeted resident URLs

M06bb adds `header_urls::Budgeted` over one admitted immutable field slice,
retaining the original job/email budgets and non-copyable private credit.
It preserves URLs/ListPost grammar, whole-field validation before replay,
ASCII Byte events, exact spelling and NO as an empty ListPost list. Only the
List-Post field may authorize that mode. No URL is fetched or resolved.
Malformed/nesting outcomes precede all events; later resource refusal retires
all provisional replay output. No property JSON is emitted here.

The shared private Conversion adapter now serves MessageIds text and URLs.
Non-output work uses Parsing's source-visits-plus-records rule with at least
one step per charge, including EOF. Output-only work first charges one step,
then job output; prior admitted work remains if the latter refuses. Mixed
output/input/record/unlink requests refuse before counters change. Moving
this adapter does not change MessageIds costs. URL, CFWS and URI validation
share the same original budgets and field-local credit. Source lookahead is
charged before reading; the plain Meter entry point keeps its exact costs.

A poll charges at most 160 source visits, 193 aggregate steps, thirteen job
records and one output byte. The fixed IPv6 parse prepays 64 aggregate steps
before parsing at most 45 retained bytes with std. `<x:>` costs ten visits,
34 steps and two output bytes; ListPost NO costs six visits, 24 steps and no
output; `<x://[::1]>` costs 24 visits, 197 steps and nine output bytes across
both passes. Unused credit is discarded between fields. Aggregate exhaustion
latches on the email budget, while job stops retain their own typed cause.
Cached completion is inert; explicit final admission remains live and can
retire it. No resource failure becomes malformed or a successful prefix.

The wrapper fits 288 bytes including inline CFWS, URI state, the 45-byte IPv6
array, budget references and private credit. It replaces the standalone URL
cursor in the existing 16 KiB parser reservation. The transient adapter uses
three references, with no arena, retained URL/list or conversion scratch.
Combined worker/native/RSS qualification remains open.

Tests compare plain/budgeted events, visits and output; pin exact two-pass and
IPv6 costs; exhaust every partial budget through validation/replay; and cover
pre-access refusal, field-credit isolation, job stops and final admission.
Probe intervals cover long comments/URLs, literal parsing, malformed tails,
NO and terminal refusal in both registered modes. Section 1.61 adds URL
property JSON; unpublished response-spool retention remains follow-on work.

### 1.61 Complete provisional URLs header values

M06bc adds `header_value::URLs` over resident Input and original job/email
budgets. Each selected field produces a JSON array of URL strings or null;
`:all` adds an outer array. Missing last/all values are null/[] respectively.
The first active turn charges one aggregate step and, for a nine-byte name,
nine comparison bytes before matching List-Post case-insensitively. Only that
field enables the NO empty-list outcome. Other names retain URLs grammar.
No URL is normalized, decoded, resolved, fetched or executed.

URLs and MessageIds use one private list coordinator with statically selected
adapters for modes, cursor events and typed errors. It replaces the earlier
MessageIds-only owner, retaining its values, charges and diagnostic scope.
A URL Byte event becomes a character only after the URL parser's complete
ASCII syntax pass; the existing JSON frame then quotes it. That frame remains
bound to one item until End and quote drainage. No trait object, URL string,
owned list or additional arena is introduced.

Malformed URL validation consumes a checked malformed-only handoff, which
requires the original stored Malformed failure and that replay has not begun.
Partial, complete, nesting and resource failures cannot use it. Successful
handoff requires Complete with no failure. Both return the original budget
borrows and discard private credit. The shared coordinator stages null only
after the malformed handoff; a null that has not fully drained cannot be
handed back as a completed value. Late failures retire all earlier output.

URL parsing retains its charged replay bytes. JSON quotes, strings, array
punctuation and null are additional output work, prepaid before staging.
Drains copy already-paid bytes without another debit. Per-poll ceilings stay
255 visits, 256 steps, sixteen job records and six external copied bytes;
URL conversion plus JSON output charges at most four bytes per poll (null),
or two for an ordinary URI byte and its JSON copy. The shared MessageIds
path retains its eight-byte combined-charge ceiling. Parser/literal errors
use URLs; framed errors use Json(URLs); post-malformed null staging can use
Work/InterpretationLimit directly. No resource cause becomes malformed.

Cached completion is inert and final explicit admission can retire it.
Selection, empty output and provisional method-retention rules match the
other property coordinators. The URLs coordinator fits 1024 bytes in the
existing 16 KiB parser reservation, including selector, parser/URI/CFWS state,
shared frame, original budgets and inline literals. It replaces standalone
URL state and needs no NFC scratch. Combined worker/native/RSS qualification
remains open.

Tests cover widths one through eight, last/all/absence, NO mode boundaries,
unchanged spellings, whitespace, literals and malformed lists. They pin
composed costs and per-poll counters, exhaust byte/step/output cutoffs, and
check nesting, late selection, deadline and consuming handoff refusal.
Existing MessageIds tests cover the shared coordinator's prior behavior.
Allocation intervals include long URLs, null/NO, short drains and late
refusal in both registered modes. Other structured forms and unpublished
response-spool retention remain follow-on work.

### 1.62 Budgeted resident address/group parsing

M06bd adds `header_addresses::Budgeted` over the original job Meter and
per-email HeaderBudget. It retains exclusive borrows, private prepaid credit
and a terminal failure alongside the existing list cursor; it cannot be
cloned. Boundaries, CFWS, phrases, mailbox scanning, obsolete routes and
addr-spec grammar all use the same private parsing admission interface.
Their public Meter entry points retain their existing events and costs.
Address-boundary errors gain a typed InterpretationLimit for propagation.

Each admission counts source visits and aggregate steps before access. Steps
are max(visits plus parser records, one), including EOF and fixed
transitions; job records are reserved in groups of sixteen by this field's
private credit. Dropping a field discards its credit. Earlier successful
charges remain when a later admission refuses. An empty list costs zero
visits, seven steps and one job record: list/item EOF costs two, empty-CFWS
checking costs three, and the after-item and final-completion turns cost one
each.

Group and mailbox extents are provisional through whole-field Complete.
Malformed candidates retain the existing deterministic raw-item recovery;
aggregate, job and nesting failures remain distinct terminal errors and
never enter that recovery. Any late failure retires earlier mailbox and
closed-group results. Aggregate exhaustion latches across fields without
stopping the job; a job stop preserves the email budget's prior charges
without exhausting it. Cached Complete is inert; explicit check_deadline
remains live and can retire it. This parser grants no SMTP recipient or
delivery authority.

The wrapper fits 800 bytes in the existing 16 KiB parser reservation,
replacing standalone list state. Each poll admits at most 161 visits, 196
aggregate steps and thirteen job records, with no output charge. No token
text, mailbox list, heap allocation or new scratch is retained. Tests
compare plain/budgeted events and visits across valid, malformed, long and
non-ASCII inputs; exhaust both partial aggregate budgets; and pin field
credit, EOF, prior charges, nesting and late refusal. Probe intervals cover
long group/name input, routes, comments, malformed recovery and terminal
refusal in both registered modes. Budgeted address text/normalization,
property JSON, response-spool publication and complete worker/native/RSS
qualification remain follow-on work.

### 1.63 Budgeted resident address text

M06be adds `header_address_text::Budgeted` for Parsed addr-spec and Fallback
text. Private constructors reuse the existing budgeted MessageIds conversion
engine with its original Meter/HeaderBudget borrows, private credit, sticky
failure and 127-transition unfolding turn. Public MessageIds construction
still selects only MessageIds semantics; no purpose selector is exposed. The
address facade yields only Scalar/Yield/Complete and is not Clone.

Parsed mode validates the complete addr-spec before any scalar, then replays
its admitted grammar parts, discards grammatical CFWS and unfolds text.
Fallback mode trims raw edge whitespace, unfolds and repairs invalid UTF-8.
Both preserve case and decomposed spelling, avoid encoded-word
interpretation and NFC, and replace noncharacters while reporting an
encoding problem. Diagnostics become final only with Complete. No delivery
authority is granted.

The shared Conversion policy charges parser visits/records, trimming, EOF
and conversion work to the aggregate budget. Intermediate unfolded bytes and
emitted UTF-8 scalar bytes are separately charged as output; each output
admission also consumes one aggregate step before job output admission. A
later refusal retains earlier charges and retires all emitted text. Resource
errors never become malformed or silently select fallback. Explicit final
admission remains live after otherwise inert cached completion.

The facade fits 448 bytes in the existing 16 KiB parser reservation,
replacing standalone address text state. Turns admit at most 160 visits, 255
steps, sixteen job records and four output bytes. There is no extra scratch
or NFC arena. Empty fallback costs one step, no visits/output and one
private job record; `a@b` costs 24/8 visits in Parsed/Fallback and six
output bytes in both (three intermediate plus three scalar bytes).

Tests compare plain/budgeted text, diagnostics, visits and output costs,
including long inputs, folds, malformed syntax and repaired UTF-8. They
exhaust partial aggregate byte/step budgets, every output cutoff for a short
address, field-credit isolation and terminal job/nesting/deadline failures.
Probe intervals cover long Parsed/Fallback text, noncharacters, encoding
repair and refusals in both registered modes. Name normalization, complete
address-property composition, unpublished response-spool retention and
combined worker/native/RSS qualification remain follow-on work.

### 1.64 Selected display-name validation and normalization

M06bf adds `header_name::Cursor` for a caller-selected Phrase or Comment
extent within one admitted field value. Construction checks the range but
reads no name text. The caller remains responsible for selecting the display
name from the mailbox/group grammar and authorizing the field/form. This
cursor never interprets an address identity or grants delivery authority.

One inline owner first validates the complete selected phrase or exactly one
parenthesized comment. It admits grammar work through the shared Parsing
policy under the original job/email budgets and retains private credit. Only
successful completion consumes the grammar cursor into its source-bound
proof. The owner then lends the same original budgets and NFC scratch to the
existing phrase/comment decoder and normalizer. Phrase proof binding checks
the exact source extent inside the original field. Validation credit is
discarded at this handoff; normalization starts its own private credit.
There is no replacement budget, alternate input or allocating transition.

The cursor emits no scalar before whole-name validation. It retains the
existing display-name rules for encoded words, phrase gaps, comment text,
character filtering, UTF-8 repair and NFC, including fixed-scratch overflow
replay. Scalar output and encoding diagnostics remain provisional until
Complete. Malformed, nesting, job and aggregate failures stay typed and
sticky. Validation admission errors carry the selected Phrase/Comment label;
normalization errors carry Normalize. No failure silently chooses another
name. A cached Complete is inert, while explicit check_deadline can still
retire it. Empty valid names remain empty names, distinct from absence
selected by the mailbox owner.

The owner fits 1280 bytes in the existing 16 KiB parser reservation and
borrows the existing 3072-byte NFC scratch. Its inline normalization state
replaces the standalone cursor in the NFC region, leaving that region's
cursor slot unused on this path. Each poll executes one bounded grammar or
normalization turn, with at most 255 visits, 256 aggregate steps and sixteen
job records. It does not charge serialized output: scalars are internal
conversion events; a future JSON coordinator must charge its actual bytes
before staging them. No name string, token list or new arena is retained.

Tests cover exact selected extents, malformed/nesting refusal before output,
encoded words, filtering, diagnostics and shared-scratch overflow replay.
Independent validation plus normalization pins composed charges and the
private-credit handoff. Exhaustive partial byte/step limits cover
validation, normalization, late scalar refusal and cross-field exhaustion.
Deadline checks cover validation, the handoff and completed values. Probe
intervals cover long phrase/comment input, overflow replay, scratch reuse
and final refusal in both registered modes. Name JSON, complete
address-property composition, response-spool publication and
worker/native/RSS qualification remain follow-on work.

### 1.65 JSON strings from budgeted address and selected-name owners

M06bg adds `json_string::Cursor::from_budgeted_address` and `from_name`.
Both borrow an unpolled source for the frame's entire lifetime and retain
its original budgets; the selected-name source also owns the scratch borrow.
They reuse the existing fixed JSON Frame. Dropping an incomplete frame
abandons the whole property: an advanced source must not be rewrapped. These
are individual strings, not complete address objects or properties.

Name framing validates and normalizes through the selected-name owner;
address framing preserves Parsed/Fallback identity behavior. Diagnostics
stay with that source and are final only after complete framing. Quotes,
escapes and UTF-8 bytes are prepaid before staging. Name scalar events add
no other output debit; address conversion retains its intermediate/scalar
debits in addition to JSON bytes. Short-buffer drains copy already-paid
bytes. The shared sources expose only private output-charging hooks for this
adapter.

Zero-byte checks remain live during validation, conversion, normalization
and staged drains. Name admission errors retain the active Phrase/Comment or
Normalize label inside JSON Name; address errors retain JSON Address.
Resource failure cannot become malformed or a partial success. Malformed
sources may leave a provisional opening quote but emit no invalid source
text; their entire property must be discarded. Cached frame completion is
inert; explicit final admission can retire it after its closing quote has
drained.

The borrowed adapter still fits 64 bytes and the common frame 32 bytes. The
source owners retain their existing bounds and scratch reservations; no
owned string, collection or arena is added. Per turn bounds remain 255
visits, 256 aggregate steps, sixteen job records and six externally copied
bytes. Address sources retain the tighter 160-visit/255-step ceiling.
Combined address conversion/JSON output charges at most eight bytes per
poll; name JSON charges at most six. Framing adds no aggregate
interpretation steps.

Tests cover widths one through eight, address identity, name normalization,
escapes, diagnostics and malformed sources. Independent source conversion
plus wire length pins exact combined costs. Every partial byte/step/output
budget retires the provisional JSON, including refusal before the closing
quote; empty output and final deadlines retain their contracts. Allocation
intervals cover long address/name inputs, original budgets and scratch,
one-byte drains and output refusal in both registered modes. Full
address-property assembly, response-spool publication and complete
worker/native/RSS qualification remain follow-on work.

### 1.66 Provisional Addresses property values

M06bh supplies `header_value::Addresses::new`, accepting the same immutable
selection Input, original job/email budgets and caller-owned NFC scratch. It
binds only Form::Addresses and inherits last/all occurrence selection,
source-end handling and missing-header null/empty-array rules from the common
owner. Each selected field yields an array of objects with `name` and `email`
keys; `:all` wraps those field arrays in wire order. Group members are
flattened; empty groups contribute no objects. Group names are
grammar-validated but are not decoded and do not contribute encoding
diagnostics to this form.

Parsed mailbox names use the selected phrase or first eligible trailing
comment, decoded and NFC-normalized; absent names are null and valid empty
names are empty strings. Address identities use Parsed conversion, preserving
case and decomposition while removing validated CFWS/routes and unfolding.
Recoverable malformed items use null names and Fallback address text. UTF-8
repair/noncharacter diagnostics come only from projected names/addresses in
selected fields. Resource errors never become raw recovery or successful
partial results. These read-only values grant no SMTP delivery authority.

Suspend the field parser around one mailbox at a time. Transfer the original
Meter/HeaderBudget and scratch into the name owner, consume it only after
actual Complete, then reuse the same budgets for address conversion. Retain
the suspended field parser's own previously funded interpretation credit;
name and address children keep their separate private credit. Return the
original budgets to selection only after the field array has fully drained.
No object list, decoded name, address string or group array is retained.

Objects, array delimiters, keys and null literals are prepaid before staging;
string frames charge their exact JSON bytes in addition to existing address
conversion output. One turn emits at most six bytes and charges at most nine
output bytes, 255 source visits, 256 aggregate steps and sixteen job records.
The coordinator fits 2560 bytes in the existing 16 KiB parser reservation,
including its selector, suspended parser, active child, frame and literals.
It borrows the existing 3072-byte NFC scratch; its inline normalization state
replaces the standalone NFC cursor slot. No memory partition grows.

All chunks and encoding diagnostics are provisional through complete field
parsing and selection. A later malformed-nesting/resource failure can retire
already emitted objects; a later selection failure can retire an earlier
whole field array. The future response owner must retain these chunks in an
unpublished spool tail and discard the entire property on failure. Cached
completion is inert, but explicit final admission remains live. Typed errors
retain selection, address grammar, name/text conversion and JSON causes.

Tests cover widths one through eight, last/all/absence, group flattening, raw
recovery, selected diagnostics, identity versus NFC, and absent/empty names.
Independent parsing/conversion plus wire-length accounting pins original
budget use and suspended parser credit. Every partial byte/step/output limit,
late nesting/selection failure, exact completion, handoff and deadline checks
retain no successful prefix. Allocation intervals cover long names/addresses,
multiple fields, scratch reuse and late refusal with one-byte drains. Grouped
Addresses, response-spool publication and worker/native/RSS qualification
remain follow-on work.

### 1.67 Provisional GroupedAddresses property values

M06bi supplies `header_value::GroupedAddresses::new`, binding the
GroupedAddresses form. It uses the same Input, original job/email budgets and
NFC scratch as Addresses. Its array contains group objects with `name` and
`addresses` keys, matching RFC 8621 section 4.1.2.4. Consecutive ordinary
mailboxes share a null-name group; explicit groups retain decoded,
NFC-normalized phrase names, including empty strings and groups containing no
addresses. Empty fields remain empty arrays. Last/all occurrence and
missing-header rules stay common to both forms. Group boundaries and malformed
recovery follow section 1.41.

A private static mode shares the existing Addresses coordinator. Preserve
BeginGroup and EndGroup around mailbox conversion, retain the current group's
name extent, flags and name-continuation phase, and consume the same name
child for group and mailbox names. Both contribute selected-field encoding
diagnostics; an unselected field contributes none. Flat Addresses continues to
omit group names and their decoding work/diagnostics. Address spelling and
mailbox-name selection are unchanged. Form choice cannot switch during a
property. Encoded words retain their original-field boundary rules: one
adjacent to the group colon without intervening whitespace remains literal and
sets no decoding diagnostic.

Prepay the group prefix, null literal, addresses key and closing brackets
before staging. Name frames charge their exact JSON bytes as they stream.
Split the fourteen-byte addresses key/opening-array literal across two turns
so the existing nine-byte staging/output ceiling holds. Group transitions run
separately from child conversion; parser credit and original budgets survive
each handoff. The existing 2560-byte coordinator bound, 3072-byte borrowed NFC
scratch, six-byte copy ceiling and other Addresses turn bounds are unchanged.
No group/name/member list is retained.

An emitted group, including a fully closed empty group, remains provisional
through field and property completion. Late parsing, conversion, selection or
budget failure retires all earlier chunks. Typed errors, consuming completion
checks, inert cached completion and live final admission retain the Addresses
contract. No response-spool publication is supplied here.

Tests cover named/unnamed transitions, empty/null names, empty groups,
malformed and missing-semicolon recovery, selected group diagnostics, NFC,
short output and last/all/absence. Independently composed grammar and
conversion costs include group-name work; partial aggregate/output budgets and
late nesting/selection failure cannot return partial success. Allocation
intervals cover long group/mailbox names, identities, multiple fields, scratch
reuse and late refusal. Complete worker/native/RSS qualification and remaining
Text grammars remain open.

### 1.68 Bounded header-form dispatch

M06bl adds `header_value::Cursor::new(input, scratch, work, budget)`. The
already-authorized `Input.property.form()` selects exactly one existing Raw,
Text, Addresses, GroupedAddresses, MessageIds, Date or URLs coordinator. A
private inline enum retains that owner for its entire lifetime. There is no
box, trait object, copied input or second active parser. Construction adds no
work charge; selected constructors retain their own checked input contracts.
The caller supplies the existing NFC scratch even for forms that do not use
it, allowing one uniform admission path and reuse after the owner is dropped.

Polling and final deadline admission forward to the selected coordinator.
Errors retain their original variants, including unsupported Text grammar;
dispatch does not authorize an additional grammar or reinterpret a refusal.
The same original job/email budgets stay borrowed through selection and
conversion. Last/all, absence, selected repair diagnostics, unverified-leap
reporting, inert cached completion and final retirement keep their existing
semantics. Date and URLs have no encoding-repair diagnostic; unverified-leap
reporting is false outside Date. Diagnostics become final only on property
Complete. Every emitted byte remains provisional under the response-spool
contract, including a closed value before a failed final deadline check.

The owner fits 2560 bytes including its discriminant, using the existing
16 KiB parser reservation; its borrowed NFC scratch remains 3072 bytes.
Forwarding calls exactly one coordinator per turn and adds no input visits,
interpretation steps, job records or output charges. Existing maxima remain
255 visits, 256 steps, sixteen job records and six copied bytes per turn.
Addresses/GroupedAddresses charge at most nine output bytes per turn; Date
can charge twenty-seven when rendering its timestamp and quotes. This does not
qualify combined worker/native memory.

Tests drive all seven forms through small output buffers, last/all and
absence, fixed state, selected diagnostics, unsupported grammar, sticky
partial-output refusal and final deadline retirement. Allocation intervals
include construction and one-byte drains for each form, NFC overflow, scratch
reuse and refusal. Structured Text grammars, MIME part traversal and the
unpublished response-spool implementation remain follow-on work.

### 1.69 Resident MIME field syntax

M06bo adds `mime_fields::Cursor` for one complete authorized resident
field value, excluding its final line ending. Kind selects ContentType,
ContentDisposition or TransferEncoding; the caller binds that choice to
the selected header. The cursor validates ASCII MIME tokens,
type/subtype, semicolon-separated name=value parameters, quoted values
and optional CFWS. It reuses the existing bounded comment and
quoted-string validators, including their UTF-8, fold, quoted-pair and
obsolete-control policy. CFWS is permitted before and after the head and
between grammatical tokens. TransferEncoding admits one token with
optional CFWS and refuses parameters. Missing tokens, trailing
semicolons, incomplete quotes and extra unprotected text are Malformed.
Comment depth above 32 is NestingLimit, an interpretation refusal.

Head carries original first/optional second token extents. Parameter
carries original name/value extents and a quoted flag; quoted value
extents include the delimiters. Events preserve case, wire order,
duplicate names and RFC 2231 suffix spelling. They neither unquote
values nor decode percent escapes, encoded words or charsets. Unknown
tokens and syntactically valid extended parameter spellings remain
syntax, not a supported media-type or filename claim. Invalid RFC 2231
candidates are a later metadata decision.

Poll emits Yield, Head, Parameter or Complete. Every event is
provisional until whole-field Complete: a malformed suffix or resource
refusal retires all earlier extents. This helper does not discover field
boundaries or choose among duplicate fields. The enclosing selector must
validate the complete candidate before applying POLICY.md's
first-valid-field rule. No field list, parameter list, source copy,
recursion or allocation is retained.

Each active poll charges before inspection and calls at most one lexical
child, or scans up to 32 token octets. The plain cursor charges at most
160 source visits and 32 records per turn. Cached Complete is inert;
failure latches even with a replacement meter. Callers supply current
Tick and cancellation checks around active turns.

Budgeted borrows the original job Meter and email HeaderBudget
throughout validation, using the existing private parsing seam. Source
revisits charge both budgets; EOF attempts and transitions charge
aggregate steps. One job record prepays at most sixteen steps with
private per-field credit. Bounds are 160 visits, 193 aggregate steps and
thirteen job records per turn; no output capacity is charged. First
Complete remains an active charged turn, including a live deadline check
after the final extent event. Thereafter, check_deadline provides live
final admission without new byte or step cost and can retire cached
completion. Its failure also latches. The owner and plain cursor each
fit 512 bytes in the existing 16 KiB parser reservation and own no
scratch arena.

Tests pin raw extents, ordering, quote/fold/UTF-8 validation,
complete-suffix failure, nesting, long inputs, original-budget
accounting, every partial aggregate admission and final deadline
retirement. Allocation intervals cover construction, long
comments/tokens/quotes and late refusal. RFC 2231 assembly, part traversal,
response retention and combined worker/native memory qualification remain
open. Section 1.70 provides first-valid metadata field selection.

### 1.70 First-valid resident MIME metadata fields

M06bq adds mime_metadata::Cursor over an authorized immutable resident header
section. Construction checks base plus source length; the caller supplies the
absolute base, raw-header byte limit, Context and SourceEnd. Eof denotes actual
source EOF. Prefix requires a definitive scanner boundary: a blank separator
or an invalid field/unattached continuation as specified in section 1.14.
An ambiguous partial name or value remains Truncated. End.body_start is
authoritative even after tentative body lookahead. This borrowed view grants
no blob/part location authority and does not make a prefix into EOF.

The owner scans once with mime_headers, compares names ASCII case insensitively
against the three fixed MIME field names and validates each eligible candidate
with mime_fields. Only complete syntax may retain its Field and Head. A
malformed occurrence is skipped; after one valid occurrence of a kind, later
duplicates are ignored. Comments, quotes and the entire parameter suffix are
validated even though this increment retains no parameter collection. Nesting,
raw-header limits, truncation, job work and aggregate interpretation refusal
retire the whole selection. They cannot select a later duplicate or default.

Context::Normal defaults an unselected type to text/plain; DigestChild defaults
it to message/rfc822. This applies after all malformed type occurrences too,
with the parser disagreement described in POLICY.md. Selected heads retain
literal case. Field offsets are absolute; Head offsets are relative to the
selected Field's exact value slice. Content-Disposition and transfer encoding
remain optional validated source views; an unknown syntactic token is valid.
Case folding, transfer-token classification, charset/boundary validation and
RFC 2231 assembly require their own bounded work. No supplied name is a path.

Poll returns only Yield or Complete. selection returns None until the entire
raw header section validates and a separately charged final completion turn
passes. After Complete it returns the fixed Selection containing the type or
its default, two optional source views and scanner End. On failure it returns
the same latched error; no earlier candidate becomes public. These events and
views are parsing results, not authorization or publication evidence.

Each plain turn performs one scanner operation, one fixed-name comparison, one
MIME syntax operation or one scalar transition. Bounds are 256 source visits
and 32 job records. The longest compared name is 25 bytes and its two sides
charge fifty visits; token validation retains its 160-visit/32-record ceiling.
No output bytes, growing field/parameter list, copied value, recursion or new
arena is added. Cursor and Budgeted each fit 1 KiB in the existing 16 KiB parser
reservation; their three selected views are fixed inline state.

Budgeted retains the original job Meter and email HeaderBudget through every
candidate and the final scanner turn. One private credit prepays at most sixteen
steps per job record. Its scanner yields after at most 255 transitions, leaving
one emission step; syntax keeps its existing admission seam. Bounds are 256
visits, 256 aggregate steps and sixteen job records per turn. Cached completion
and selection access are inert. Budgeted::check_deadline supplies fresh final
admission and can retire cached metadata; its failure latches across later
calls. Plain Cursor callers check fresh admission externally. Callers retain
fresh clocks, cancellation, source authorization and publication checks.
Neither live owner is Clone or Copy.

Tests cover independent kinds, case/comment/fold preservation, malformed tails,
first-valid duplicates, defaults, prefix truncation/separators, nonzero bases,
invalid ranges, long Unicode, late header/nesting refusal, all partial work and
aggregate cuts, final deadlines and cached-result retirement. Allocation probe
intervals cover constructor/selection/drains/defaults and late refusals. These
are component evidence; MIME value derivation, traversal, response retention
and complete worker/native memory qualification remain open.

### 1.71 Bounded MIME parameter-name classification

M06br adds mime_attribute::Cursor as a thin mail facade over the shared
std-only td-header classifier. The caller supplies a complete authorized
parameter-name slice, normally replayed from a selected complete MIME field.
The classifier grants no field, blob, value or publication authority.

Name returns an original base-name Extent relative to that slice and Form:
Ordinary, Extended, Section { index: u64, encoded: bool } or Malformed.
Ordinary names use ASCII MIME-token syntax. At the first star, the base is
fixed and extended attribute syntax additionally excludes apostrophe and
percent. A bare final star is Extended. A decimal suffix has no leading
zero except the single digit zero; one final star marks an encoded section.
Empty bases, extra suffix bytes, invalid token bytes and checked u64 index
overflow are Malformed. Malformed retains the base extent so a later
candidate selector must reject that entire matching extended series.

Classification preserves spelling and does not concatenate sections,
validate duplicates/gaps, parse charset/language prefixes, percent-decode,
unquote, decode words, normalize display text or apply fallback. An index
within u64 is lexical evidence, not permission to allocate an index-sized
array or to accept a missing segment. Values and candidate assembly retain
their own complete-validation and original-budget requirements.

Each plain turn checks at most 32 bytes and charges at most 32 source visits
and 32 job records. EOF completion is a charged transition, even for an
empty name. Work failures latch across replacement meters; cached completion
is inert. The shared helper uses only a bounded caller Work callback, no
mail, clock, I/O or heap ownership. Neither live owner is Clone or Copy.

Budgeted retains the original job Meter and HeaderBudget, with private
credit across all turns. Every source byte charges two aggregate steps
(inspection and transition); final EOF charges one. Per-turn ceilings are
32 visits, 64 aggregate steps and four job records. No output-byte capacity
is charged. Budgeted::check_deadline binds shared check_work to zero-count
admission through the same job/email owners and can retire cached completion.
That refusal latches in the shared cursor; there is no second error cache.
Plain callers use check_deadline(now, work) for fresh admission; refusal
retires the same cursor. Cursor fits 128 bytes and Budgeted fits 160 in the
existing 16 KiB parser region. No collection, source
copy, heap allocation or scratch arena is introduced.

Shared fixtures pin suffix forms, ordinary/extended attribute differences,
leading zeros, overflow, long names, exact costs and every callback cut.
Mail fixtures pin case/extents, budgeted turns and partial aggregate refusal,
EOF deadline and cached retirement. Allocation intervals cover constructor,
long replay, malformed spelling/overflow and late refusal. Parameter-value
assembly, MIME part traversal and combined worker/native qualification remain
open.

### 1.72 Bounded MIME parameter octets

M06bs adds mime_value::Cursor as a thin facade over std-only
td-header::mime_value. Supply one complete raw parameter value, including
quotes when quoted=true, from an authorized complete selected MIME field.
The helper validates the whole token/quoted spelling first, then returns
provisional Octet { role, value } events. Complete covers this value only;
no field, candidate, blob, boundary, charset or publication authority follows.
Every event retires if later value, candidate or enclosing derivation fails.

Mode::Ordinary removes quoted delimiters/pairs and unfolds logical CRLF or
bare LF followed by SP/HTAB; it preserves the following whitespace. The
quoted lexical validator retains its existing UTF-8/control rules. An
ordinary unquoted value is a nonempty ASCII MIME token; a quoted value may
be empty. No trimming, encoded-word decoding, scalar filtering or NFC occurs.
Escaped CR/LF are unfolded after unquoting using the same logical-fold rule.
Without following WSP they remain literal octets for later control filtering.

ExtendedInitial additionally requires charset'language'data. Both apostrophe
delimiters are mandatory even when either label is empty. Roles distinguish
Charset, Language and Data octets. Charset label bytes use attribute-char
syntax; empty or unknown labels do not establish a charset default. Language
uses the existing lexical 1..8-letter primary and hyphenated 1..8-letter/digit
subtag rule through shared language_tag::Tag, with no registry lookup.
Encoded-word qualifiers use that same feed state under their existing
precharged bounded recognition. ExtendedContinuation has data only.
After unquoting/unfolding, data consists of ASCII attribute chars or exact
percent triplets. Both hexadecimal cases are admitted. Quoted wrappers are
an explicit compatibility admission; spaces or other forbidden logical bytes
still invalidate extended data unless percent encoded. Triplets cannot span
separate values/sections. The cursor emits decoded octets including NUL and
invalid UTF-8; charset conversion/filtering and cross-section scalar state
belong to the enclosing owner. Bytes introduced by this projection never
authorize encoded-word recognition. Ordinary filename/name compatibility
retains a separate original-source placement path.

The shared Work callback admits before each source access or transition.
Plain turns cap visits at 160 and records at 32; projection emits at most
one octet per turn. Validation and logical fold lookahead/rereads are charged.
The mail Budgeted owner retains the same original Meter, HeaderBudget and
private record credit; turns cap visits at 160, aggregate steps at 192 and
job records at 12. Octet events are parsing evidence, with no retained output
buffer, so these helpers charge no output-byte capacity. The owner charges
output before copying bytes into retained materialization. Cursor fits 160
bytes and Budgeted fits 192 in the existing 16 KiB parser reservation.

Live owners are neither Clone nor Copy. Cached completion is inert. Plain
check_deadline(now, work) and Budgeted::check_deadline(now) explicitly bind
zero-count live admission to shared check_work, latching refusal in that
same shared cursor even after completion. Replacement meters/callbacks cannot
revive any failure. No additional failure cache, collection or heap backing
is introduced. Numbered candidate replay, duplicate/gap validation, display
conversion, body-part traversal and full worker qualification remain open.

### 1.73 Shared unquote/fold projection

M06bt replaces the three value, phrase and comment logical readers with
std-only td-header::projection::atom. It receives a position, quoted flag
and caller read callback; the callback enforces admitted source bounds and
EOF policy before access. At most six callback calls project one Octet with
value, next source position and escaped provenance. The helper owns no
source, cursor, work, clock, output backing or lexical validity proof.
It operates only on complete spelling already validated by its caller.

Unquote first, then unfold logical CRLF or bare LF followed by SP/HTAB.
Keep the following whitespace and OR escaped provenance across all fold
contributors. Escaped nonfold line endings remain literal for later control
filtering. These bytes grant no original-source encoded-word placement.
Comment callers retain that provenance; phrase and MIME value callers retain
their existing separate placement or literal-octet rules.

Read errors return immediately. IncompletePair and position overflow are
separate typed failures: value maps incomplete pairs to Malformed, while
validated phrase/comment consumers map invariant failure to InvalidState.
Each enclosing owner retains its existing sticky refusal. Value/comment
callbacks preserve charged zero-byte EOF attempts; phrase callbacks preserve
the token-end check before charged access. Original Meter/HeaderBudget and
private credit remain in the enclosing adapters. No turn/cursor/reservation
bound changes and no new backing is introduced. Existing allocation probes
cover all three migrated consumers. Shared fixtures pin exact read positions,
maximal calls, escape provenance, every read/EOF refusal, all octets and
incomplete/overflow behavior. Mail fixtures pin the differing EOF charge
contracts and existing placement behavior. Full worker/native qualification
and MIME traversal remain open.

### 1.74 Complete bounded MIME parameter-family selection

M06bu adds mime_parameter::Cursor over one immutable complete MIME field
value and its ContentType or ContentDisposition kind. Attribute selects
Boundary, Charset, Name or Filename through trusted fixed ASCII names;
matching is case insensitive and charges its source reads. TransferEncoding
is refused as InvalidState. The caller retains the original field-selection
context and the meaning of kind/attribute pairs; any of the four names may
be selected from either parameterized kind. Complete is parsing evidence,
not a boundary, charset, display
string, blob, path or publication capability.

The first pass validates the whole field and classifies every name. Retain
the first ordinary value as one Parameter extent of source offsets. A single extended
value and numbered sections are alternative families: duplicate singles,
mixed forms, malformed matching names, duplicate indices or gaps reject the
whole extended family. Section numbers begin at zero. All-unencoded numbered
sections also form a continuation family and precede ordinary fallback.
The necessary max+1=count check rejects enormous or plainly incomplete
indices without index-sized work; it does not prove uniqueness.

Replay the complete field for each increasing index and require exactly one
matching section. Ordinary values are already validated by the whole-field
grammar. Fully validate extended/section values through mime_value;
encoded zero requires charset'language' prefix delimiters, later encoded
sections require complete percent triplets, and unencoded sections remain
logical ordinary octets. Triplets cannot cross sections. All values must
complete before Plan is published; no partial decoded bytes are retained.
Malformed extended values reject the family; complete ordinary fallback
remains available. Whole-field syntax/nesting and every work or aggregate
refusal instead retire the entire selection. None of those failures can
become a fallback success.

Status::Complete(Selection) supplies plan and invalid_extended. A plan is an
Ordinary/Extended Parameter or Sections { count, initial_encoded }; the
passive flag distinguishes absence from rejected extended spelling without
allocating diagnostics. It grants no word-placement or metadata authority.
A later drain must replay those exact sections under the original budgets
and keep conversion output provisional. Unquoting or percent decoding never
creates original-source word placement. Charset/scalar state crosses mixed
sections only in that later owner.

One poll executes one child turn or fixed transition. Plain turns cap visits
at 160 and records at 33; Budgeted turns cap visits at 160, aggregate steps at
256 and job records at 16. All replay and comparisons are charged; quadratic
replay is explicit and bounded by aggregate allowance, not an arbitrary
linear-time claim. Cursor fits 1024 bytes and Budgeted 1056 inside the existing
16 KiB parser reservation, with no section vector, copied source or output
backing. Live owners are neither Clone nor Copy. Final completion is charged;
cached results are inert. Explicit fresh check_deadline uses zero-count live
admission and latches refusal, including after cached completion and across
replacement plain meters. The budgeted owner preserves the original Meter,
HeaderBudget and private credit. Constructor, long replay, invalid/fallback
and late refusal are allocation-probe intervals. Display/boundary/charset
projection, MIME traversal and worker/native qualification remain open.

### 1.75 Validated MIME parameter octet replay

M06bv adds mime_parameter::Octets and BudgetedOctets. Construct them from
one immutable complete field, field kind and trusted Attribute, never a
caller-manufactured Plan. One inline Cursor first validates the entire
family as in section 1.74, discarding all octet events. Only after complete
selection does that same inline state reset for charged replay. The live
Meter/HeaderBudget and private credit never reset.

Ordinary and single extended values replay their exact selected raw extent.
Numbered values replay the whole field for each increasing index and keep
uniqueness, syntax and mode checks. All replay reads are charged. A rejected
extended family produces only its selected ordinary fallback; bytes or labels
from the rejected family cannot escape the validation phase. Missing values
complete without octets. A replay refusal retires the entire derived output;
replay cannot choose a second fallback or complete a truncated family.

OctetStatus yields at most one Charset, Language or Data octet per turn. Its
role carries lexical evidence only. Original-case and empty/unknown labels
remain intact. Mixed sections retain one ordered Data stream, so a later
charset state may span them; complete percent triplets remain section local.
All-unencoded series emit Data only. No decoded data gains RFC 2047 placement
authority; the final Selection preserves ordinary raw extents for a later
original-source compatibility path. Do not run a Text placement recognizer on
unquoted, percent-decoded or joined output.

Every emitted byte is provisional until charged OctetStatus::Complete returns
the original Selection and its passive rejection diagnostic. Output retention,
charset choice and conversion, filtering/NFC, boundary admission and metadata
publication remain the caller's later responsibilities. An unknown/empty label
is not a charset default; POLICY.md specifies later replacement recovery.

Owners are neither Clone nor Copy. Octets fits 1088 bytes and BudgetedOctets
1120 in the existing 16 KiB parser reservation, with no copied source, section
vector, owned label or decoded backing. The selector's fixed turn limits
remain 160 visits and 33 plain records, or 160 visits, 256 aggregate steps and
16 budgeted job records. Resetting inline replay state adds no work allowance.
Cached completion is inert; explicit zero-count check_deadline uses the same
live allowances and latches refusal after completion or replacement meters.
Allocation intervals cover constructor, complete selection, long replay,
fallback, empty/rejected families and sticky late refusal. These are resident
parser/Rust-allocation claims; full-worker, native/RSS and traversal
qualification remain open.

### 1.76 Bounded literal MIME parameter scalar conversion

M06bw adds mime_parameter::scalars::Cursor and Budgeted for trusted Name
or Filename attributes in a complete immutable parameterized field. Other
attributes are refused as InvalidState after the first charged admission.
Work/aggregate refusal takes precedence over inspecting that contract;
protocol field/attribute meaning
remains caller owned. The owner composes complete selection and octet replay
from section 1.75 with one fixed-state charset Decoder. It accepts original
source/kind/attribute, never a manufactured plan or caller-decoded byte slice.

Charset::parse and incremental mime_charset::Label use one exact ten-alias
table for UTF-8, ASCII, Latin1 and Windows1252. Matching is ASCII case
insensitive, never trims whitespace and assigns no default. Label retains
ten candidate flags and a saturating bounded position, no label buffer or
registry lookup. One feed performs at most ten trusted alias comparisons;
the enclosing conversion owner charges its record work before feeding an
already charged octet. Existing body/word alias choices remain unchanged.

Declared initial charset labels select that decoder. Unknown or empty labels
use explicit UTF-8 replacement recovery with a diagnostic. Unlabelled display
values use native UTF-8 conversion, including an unencoded zero followed by
encoded later sections; that is display recovery rather than an RFC charset
default. Keep one decoder across every ordered Data octet, including literal
UTF-8 source bytes in unencoded sections under a declared single-byte charset.
Never choose a different charset for each section or reset at a section edge.
Incomplete EOF and malformed sequences replace under the existing decoder
policy; invalid continuation lookahead is charged and revisited.

One fixed held octet connects the replay owner and decoder. Each turn polls
one child or executes one fixed phase; at most one Scalar(char) is emitted.
Charset/language bytes never enter data conversion. Language remains passive
validated metadata. Scalar controls, noncharacters and word-looking bytes
remain literal here: filtering, noncharacter recovery, NFC and ordinary
RFC 2047 compatibility belong to later original-source display owners.
Converted or joined bytes create no word-placement authority.

All scalars are provisional until charged Complete(Decoded) returns the
original Selection and is_encoding_problem. That flag records only charset
label recovery or malformed charset data; invalid_extended remains separate
in Selection for the later diagnostic owner. Absence returns no scalars and
no charset diagnostic. Final fresh admission, original job/header allowance
and private credit persist across selection, replay and conversion. Every
refusal retires the whole scalar derivation; cached completion is inert and
explicit zero-count check_deadline latches late refusal, including replacement
plain meters. Output capacity and retained copying are charged by the later
owner; this producer allocates and retains no scalar string.

Label fits 16 bytes, Cursor 1280 and Budgeted 1312 in the existing 16 KiB
parser reservation. Live owners are neither Clone nor Copy. Turns remain
bounded at 160 source visits/33 plain records or 160 visits/256 aggregate
steps/16 budgeted job records. Constructor, long conversion, known/unknown/
empty labels, invalid/fallback data and late refusal are Rust-allocation
intervals. Complete display metadata, output/NFC retention, boundary admission,
MIME traversal and full worker/native/RSS qualification remain open.

### 1.77 Original-source MIME parameter display scalars

M06bx adds mime_parameter::display::Cursor and Budgeted for trusted Name
and Filename. Construction binds the original immutable field, kind and
attribute; no public manufactured plan or decoded byte-vector entry exists.
It composes section 1.76's complete family choice and literal conversion,
then filters literal scalars. After complete selection, a private handoff
may replace the inline literal owner with a decoder for the exact selected
Ordinary raw extent, before any Data conversion. No second selector,
allowance or candidate fallback is created. That internal handoff requires
no failure, phase Read, no held octet and no chosen decoder.

Only an Ordinary quoted value admits the filename/name RFC 2047 compatibility
rule. Its original outer quote edges and actual unescaped SP/HTAB or accepted
folds delimit complete contiguous candidates; Context::Text syntax applies.
Scan original bytes, reject a raw backslash in a candidate, and retain unknown,
malformed, incorrectly placed or over-75-byte words literally. Quoted-pair
removal cannot manufacture a marker or either word boundary. Literal content
uses shared unquote/unfold projection after placement. Source projection and
local UTF-8 verification are separately charged input passes; complete source
characters finish before another boundary is admitted. Preserve all leading
and trailing interior whitespace; suppress pure original unescaped LWS only
between recognized words, including words producing no surviving scalars.
Decode each word separately with the existing charset/transfer fault and
encoded-control/noncharacter policy. A recognized word's output never changes
placement for the remaining original source.

Extended and numbered families retain literal scalar conversion. A word
spelling from percent decoding, unquoting or joined sections remains literal.
Drop literal NUL after charset conversion, retain other literal controls and
replace noncharacters with U+FFFD plus a diagnostic. RFC 2231 percent-introduced
controls have this literal parameter policy; only RFC 2047 words discard the
whole encoded control ranges. Keep Selection.invalid_extended separate from
charset/filter is_encoding_problem. NFC, filename precedence across fields,
retained output/capacity and metadata publication remain later-owner work.

At most one provisional scalar escapes per poll. Original job/header work and
private credit span selection, handoff, recognition and conversion. Charged
Finish returns Complete(Decoded); every refusal drops active decoder state
and clears the provisional result, with no fallback or later publication.
Cached completion is inert; explicit zero-count check_deadline latches fresh
refusal, including after completion or replacement plain meters. Output
copying/capacity must be charged by the later retention owner.

The private ordinary decoder fits 256 bytes, Cursor 1504 and Budgeted 1536 in
the existing 16 KiB parser reservation. The active branch remains inline, with
no boxes, buffers, copied labels or scalar string; live owners are neither
Clone nor Copy. Each turn polls one child or does one fixed phase. Bounds are
225 source visits/228 plain records or 225 visits/453 aggregate steps/29 job
records; the 75-byte recognition fixture reaches 225 visits and 453 steps.
Constructor, long Unicode/labels, maximal words, escaped placement, malformed
and empty/fallback values, Name and late refusal are Rust-allocation intervals.
Complete worker stack, native/RSS, NFC/output retention, selected boundary and
MIME traversal qualification remain open.

### 1.78 Shared charged character projection

M06by replaces strict logical UTF-8 assembly in phrase, comment and ordinary
MIME parameter display readers with td-header::projection::character. The
stateless helper composes up to four shared atoms in a four-byte local
array. Separate source-read and local-verification callbacks borrow the same
original caller context sequentially. Verification admits one local
inspection of one to four bytes before strict decoding; at most 24
source-read callback calls and one verification callback occur. Caller EOF
charges, errors, clock, job/header allowances and private credit remain
unchanged.

The result contains a scalar, next source position and the first logical
octet's escape provenance, including escaped folds. Continuation escapes
never change the first octet's provenance. Invalid/truncated logical UTF-8
is a typed error; source-read and local-verification refusals remain
distinct. One private mail adapter owns the shared verification charge and
exhaustively maps errors, preserving the caller error and invariant outcome.
Mail callers map spelling invariant failures to their existing InvalidState
and latch refusal in the original owner. No new source validity or
original-placement proof follows. NUL, controls and noncharacters remain
scalars for caller policy.

All eight existing mail noncharacter checks now share the table-free Unicode
predicate. Their different NUL/encoded-control filtering and
diagnostic/output rules remain in their original owners. Placement machines
and normalization are unchanged. No live cursor, quota, output backing or
dependency is added; existing turn/cursor bounds and allocation intervals
remain the contract.

### 1.79 Shared bounded canonical composition

M06bz extracts the existing NFC ordering/composition engine into std-only
td-nfc. nfc::Scratch and Status re-export the shared types; public mail entry
points, errors, original work/header references, private credit and real
supplied Tick remain unchanged. A private context admits each engine
transition and invokes the existing charged source/decomposition reader.
Unicode 17 lookup/decomposition/classification/composition stays in mail.

The shared Source is a quota-free Copy checkpoint whose identity includes
exact source/decoder progress and pending decomposition. A generic Reader
borrows the original admission context for each bounded source operation.
No clock, meter, credit or retained output owner lives in checkpoints.
At most 1..=32 admitted transitions emit one provisional scalar; mail retains
its one-transition header and 32-transition valid-UTF8 policies. The shared
algorithm preserves stable ordering, blocking, starter composition and exact
unfinished-segment replay without completed-prefix scans. Its Unicode result
is conditional on the caller's deterministic canonical source/table contract.

The shared owner exclusively borrows the same 3072-byte scratch and is neither
Clone nor Copy. Generic size depends on Source/Error; mail still pins Source
at most 256 bytes and Cursor plus HeaderBudget at 1024. Passive borrowed inspection
supports existing adversarial checkpoint tests without a mutable restore API
or source/output validity authority. No larger parameter source is added to
this fixed header enum. A later parameter owner must qualify its own state.

poll/check failures remain sticky across replacement contexts. Cached Complete
is inert; the mail output-charge method binds fresh admission through check,
including after completion. Scratch release requires healthy Done; Done may
coincide with the final Scalar, whose output admission/copy still belongs to
the caller before publication. Mail retains its original error mapping and
whole-property discard rule. Existing official Unicode vectors, fast/overflow
replay, pending decomposition, prefix visits, aggregate/job cuts, deadlines and
allocation intervals remain consumer qualification. Standalone shared tests
qualify generic engine ownership/admission/replay, not a new Unicode database
or complete worker/native/RSS bounds.

The new internal path dependency joins ordinary roster discovery and has its
own package-only lock. All five local crypto-consumer manifests/locks and the
active graph remain explicitly pinned; td-nfc is std-only, not an external
crypto admission. Preparation/staging uses the same compiled local-source
list, with build scripts refused. Routing checks the primitive and mail reader
without claiming distribution image or portable-artifact qualification.

### 1.80 Original-source MIME parameter NFC

M06ca composes display::normalized::Cursor with the shared td-nfc engine.
Construction takes original field bytes, Kind/Attribute, existing exclusive
Scratch and original Meter/HeaderBudget. It captures only healthy pure
initial progress without inspecting source bytes. Each poll performs one
admitted engine transition and at most one existing display/decomposition
read. Scalar events remain provisional until the complete original candidate
and normalization drain, output charging and fresh final admission. Complete
carries the existing passive Decoded selection/diagnostics, not metadata or
filename publication authority. Filename precedence/retention remains open.

Opaque shared lexical checkpoints and private field/family/octet/scalar/
display checkpoints preserve every phase, nested progress, word/charset
state, held octet and decomposition. They contain no live allowance, credit,
clock or output owner. Refused owners cannot capture checkpoints. Resume
reconstructs exact pure progress and charges every subsequent access again;
it cannot revive the enclosing failed normalization owner. All live public
cursors remain non-Clone/non-Copy. Ordinary compatibility and extended-family
selection/decoding retain their existing grammar and work contracts.

The separate parameter Source is at most 1088 bytes; Cursor plus HeaderBudget
is at most 4608. Cursor, two overlapping temporary Source copies, reconstructed
display owner, live context and HeaderBudget fit the existing 16 KiB parser
reservation. The exclusive 3072-byte NFC scratch remains in the conversion
reservation. Existing header Source <=256 and Cursor plus HeaderBudget <=1024
remain unchanged; no larger variant enters that enum. These layout/allocation
intervals do not qualify complete worker/native stack or RSS.

The source has one fixed transition per poll. Display reads retain their
225-visit/453-step ceilings; engine/source/decomposition/class admission bounds
the composed turn at 225 visits, 457 aggregate steps and 30 job records.
These are conservative ceilings, not asserted simultaneous peaks. Copies
restore exact successful display-turn identity plus pending decomposition,
never normalized completed-prefix scans or a refreshed work credit. Unicode
17 decomposition/class/composition rules and the 55-class replay bound stay
unchanged. Selection and source diagnostics are retained only after successful
complete source processing; any normalization, parsing, output or fresh
admission refusal clears private result and latches the original typed error.
Cached Complete is inert. finish(now) obtains fresh admission, consumes
healthy Done and returns original work/header/scratch references; the owner
must serialize/charge its final scalar before release and obtain enclosing
publication admission. Exhaustion preserves Parameter or Normalization error
provenance according to which admission layer refuses; either is fatal.

Shared checkpoint event/cost/cut fixtures, mail nested checkpoint traces,
ordinary/extended/continued/word NFC, long combining replay, original quota
cuts, final output/deadline cuts and Rust allocation intervals qualify this
composition. No external dependency, manifest/lock, feature, unsafe, crypto
backend or staging-policy change is introduced.

### 1.81 Retained MIME filename precedence

M06cb adds mime_filename::Cursor over exact first-valid selected disposition
and type field values. Those borrowed views and their entity/header authority
belong to the metadata owner. Fields must never include later duplicates.
The cursor tries disposition Filename first and type Name only when no plan
was selected. Each field's selector retains its complete extended-family over
first ordinary preference. Malformed extended-family diagnostics accumulate
across the attempted fields, separately from final charset/display diagnostics.
A present empty candidate, including one emptied by scalar filtering, wins;
absence is not inferred from normalized output length. Malformed whole-field,
nesting, work and interpretation refusals retire the cursor without fallback.

The owner borrows original Meter/HeaderBudget/Scratch and caller-reserved UTF-8
output backing. It owns at most one parameter normalization cursor. Each field
transition is admitted against the original aggregate allowance; every source
turn retains M06ca's ceilings. Normalized scalars are output-charged before
copying whole UTF-8 encodings into checked backing. Insufficient backing is
OutputCapacity, never truncation or a reason to select a lower-priority name.
No source, label, intermediate value or output Vec/String is allocated here.

value() returns validated UTF-8 bytes without rescanning them, and no bytes
before Complete. It distinguishes absent from present
empty. Complete carries Origin, exact UTF-8 byte length and passive diagnostics.
Final source completion receives fresh admission before release; cached
Complete/value are inert and cannot authorize publication after time passes.
check_deadline obtains fresh admission and hides the value on any refusal,
including after cached completion. Refusal preserves its original typed error:
Admission carries every original work/deadline/interpretation refusal across
all phases; Decode preserves source syntax/normalization invariant errors.
Both are fatal; either retires the borrowed source owner and clears logical output length/result.
Backing bytes are not erased; this is logical retirement, not secret erasure.
The enclosing metadata/job owner must obtain final admission before publication.
A returned display name is never a filesystem path or blob/locator authority.
finish(now) requires healthy Complete and fresh admission, returning passive
Retained UTF-8 bytes/End plus original work/header/scratch references. This
permits further metadata under those original owners without copying the name
or rebuilding a slice from a saved End. Failed or premature finish yields no
retained view. All handed-off results remain provisional at the enclosing job
boundary and require its fresh final publication admission.

RESOURCES.md owns the cursor, scratch and caller-backing reservations and
capacity_bound(field_bytes)'s checked conservative UTF-8 size bound. The
invalid_extended flag aggregates rejected families in any attempted field;
Origin names only the selected field. POLICY.md owns display-name use.
Complete worker/native/RSS qualification and MIME part traversal remain open.

### 1.82 Selected boundary and charset metadata

M06cc adds mime_parameter::protocol::Cursor over one exact first-valid
Content-Type field value selected by the enclosing metadata owner. Purpose
selects Boundary or Charset and the existing RFC 2231 family owner supplies
the complete extended family, otherwise first ordinary candidate. Admission
validates the selected logical Data octets; a grammatically invalid selected
value never reconsiders an ordinary sibling. Selection.invalid_extended
still describes rejected families before candidate choice.

Complete distinguishes Absent (no plan), Invalid (selected value fails
protocol grammar) and Present. Only Present exposes exact ASCII bytes in
caller-reserved backing. No whitespace trim, charset conversion, encoded-word
recognition, scalar filtering or NFC changes structural metadata. Boundary
uses td-header's complete ASCII length/alphabet/final-byte validator. Charset
uses its nonempty token validator; the original spelling remains visible.
known_charset classifies the explicit mail aliases when Present and Purpose
is Charset, with no default. Unknown valid tokens remain Present with None;
Invalid and Absent do not silently become body charset defaults.

RFC 2231 Charset/Language roles never enter Data. A qualified family with an
empty or unknown charset qualifier sets unsupported_qualifier; admitted
ASCII Data remains exact (all supported charsets agree on these octets).
Non-ASCII Data fails protocol grammar rather than acquiring repaired syntax.
Body recovery/defaulting and multipart authority belong to future enclosing
owners under POLICY.md; these diagnostic results authorize neither.

The cursor keeps original Meter, HeaderBudget and prepaid credit through
complete-field validation, family replay, ASCII classification and fixed
alias matching. Charge classification before feeding and output before
copying. A full output window stops retention while complete grammar validation
continues. Invalid grammar wins over capacity; only a valid oversized value
returns fatal OutputCapacity. No truncation or lower-priority fallback.
Malformed whole fields, nesting or resource refusal are sticky fatal errors
and hide output. Complete/value are inert caches; check_deadline performs
fresh original admission, including after completion, and hides results on
refusal. finish(now) requires healthy completion and fresh admission and
returns passive Retained plus the exact original budgets. Failed/premature
handoff yields no bytes. Publication still needs the enclosing owner's fresh
final admission after all metadata/structure succeeds. Backing is not erased.
RESOURCES.md owns layout, work ceilings and bounded output reservations.

### 1.83 Resident MIME delimiter scanning

M06cd adds mime_delimiter::Cursor over one complete authorized resident entity
body, checked absolute base and one boundary. Source completeness/authorization
belong to the enclosing owner. Constructor checks range and boundary length;
original-work polls validate the entire boundary grammar before body scanning.
The shared td-header::mime_boundary::Line owns only pure prefix/suffix state;
mail owns source extents, accepted file endings, work, deadline and retirement.

Scan raw line starts for -- followed immediately by the complete
selected boundary. A suffix beginning with -- closes; SP/HTAB transport
padding is ignored, other suffix bytes set ignored_suffix and do not
change deterministic prefix recovery. CRLF and LF end lines, excluding
those endings from classification; bare CR stays data. Preserve a
pending CR across bounded turns. Definitive entity EOF classifies an
unterminated final line. No allocating line buffer or Unicode/transfer
decoder participates.

Delimiter reports checked absolute line_start, after_line and preceding_end,
which excludes only the previous accepted line ending. A future child owner
must clamp preceding_end at its child start for an immediately following
delimiter. Events remain provisional offsets, never complete-tree, leaf,
partId or blob authority; they cannot revoke previously copied passive data.
The enclosing traversal must discard every provisional event on later error.
Closing events do not stop this raw scanner; the enclosing container owns
preamble/epilogue, active-boundary precedence and missing-close recovery.

Every active poll funds bounded body/boundary accesses using the original Meter.
HeaderBudget is not charged for raw body bytes. Work refusal is sticky and
prevents more events; check_deadline rechecks original admission, even after
Complete. Cached Complete is inert; finish(now) requires healthy Complete
and fresh admission and returns the original Meter. Final structure publication
still requires the enclosing owner's admission after all other work succeeds.
RESOURCES.md owns fixed work/layout ceilings. Parent-first extent clipping,
header counting, explicit DFS, transfer sizes and locator issuance remain open.

### 1.84 Private polling composition

M06ce extracts protocol::Reader and mime_delimiter::Core as crate-private
progress engines without live budget or retained-output references. Public
protocol and delimiter cursors retain their original exclusive borrows,
output handling, sticky refusal, inert completion and fresh handoff, driving
the same engines. No new public mail parser or detached admission API exists.

Protocol Reader emits provisional Data octets and the same completed End.
Its owner supplies original Parsing admission and preserves prepaid credit;
only the owner funds retained output and handles capacity overflow. A
composing owner must discard provisional octets after late invalidity or
refusal. The delimiter Core retains immutable source progress, accepts a
boundary view per poll and requires its owner to pin the same immutable
bytes throughout. The public cursor pins that view by borrowing it. Shared
td-header::mime_boundary::State contains only pure prefix/suffix progress;
Line wraps that state and its borrowed view. Detached state does not grant
source, budget or publication authority.

Metadata Cursor's crate-private poll_in_context uses the original shared
HeaderBudget and prepaid credit supplied by its owner. Budgeted delegates
to it, preserving fresh live admission and cached-completion semantics.
These compositional hooks add no arena, wire output, descriptors or traversal
claim. RESOURCES.md retains the previous public work/layout bounds.

### 1.85 Complete resident MIME traversal

M06cf adds mime_structure::Cursor over one complete authorized resident
entity, checked absolute base, configured structural bounds and caller
Part slots. SourceEnd::Prefix refuses before parsing. The constructor checks
only the relevant depth/part/header bounds; full startup resource planning
and source authorization remain the caller's responsibilities. Depth counts
the root as one, parts count containers and leaves, and recognized entity
headers count exactly once across the parse, excluding separators/body and
an enclosing delimiter's accepted preceding ending.

Fixed frames retain each active immutable boundary and its private scanner.
Before child headers begin, the parent scanner finds the child's ending and
clips its source. That realizes outermost-boundary precedence, including
prefix collisions, without decoded punctuation or a growing active-boundary
list. Each parent resumes its original scanner after the child finishes.
Preamble/epilogue stay in raw extents but never become children. An opening
followed immediately by another delimiter yields an empty child, clamping
the ending at its start. Missing closes end the last child at enclosing
extent EOF with MISSING_CLOSE. Without any valid opening, or with an absent
or protocol-invalid selected boundary, return NotParsable. Known base64/QP
on multipart also returns NotParsable; no decoded structural locator stage
is invented. Unknown transfer encodings use identity with UNKNOWN_ENCODING.

Part cells report checked entity/body/type-field source extents, preorder
ordinal, parent ordinal (zero for root), depth, coarse Media, Encoding,
exact size and bounded diagnostics. Field type extents preserve complete
original spelling for later metadata projection; defaults have zero extents.
Media is a traversal classification, not a complete normalized MIME type.
Leaves count exact stable transfer-decoded octets; containers count their
identity body extents, including own delimiters and child headers, rather
than summing child sizes. Base64/QP reuse their fixed decoders and original
work/output admission. Charset, Unicode and display filters cannot affect
sizes. message/rfc822 and message/global remain leaves; a digest child with
no valid type defaults to message/rfc822. M06di preserves the original
per-part default context for retained metadata; see 1.114.

The cursor exclusively borrows original Meter/HeaderBudget and owns
prepaid credit throughout all entities and phases. Every active turn
freshly checks admission and funds bounded structural work; source
reads, parameter replay, comparisons, retained boundary bytes, decoded
counting bytes and descriptor copies spend the same original counters.
Raw body scanning does not consume header interpretation allowance.
Structural limits, backing capacity and resource refusal are typed and
sticky. No partial descriptor list is exposed; parts/header_bytes appear
only after healthy Complete and disappear on explicit late refusal.
Backing slots are not wiped on refusal or abandonment; after borrows end
they may contain provisional cells, which confer no completion or
authorization. The caller publishes only a healthy completion. Cached
Complete is inert. check_deadline and finish(now) require fresh original
admission; finish returns passive slots and the same original budgets. A
failed consuming finish releases its borrows and retains no result;
original budgets stay caller-owned without reset, and any recorded
budget refusal remains sticky. Copied passive cells cannot be revoked
and grant no blob ID, partId, source authorization or publication
authority.

RESOURCES.md qualifies bounded resident state/turns and Rust allocations.
Complete part metadata/JSON, body-list derivation, authenticated locator
issuance, streaming inputs, worker/native stack and RSS remain open.

### 1.86 Retained selected part headers

M06cg adds mime_part_headers::Cursor. Entity supplies the exact
authorized complete resident entity, absolute base, original per-section
header limit and normal/digest-child context. Captured prefixes are
refused; source identity, enclosing clipping and actual authorization
remain caller duties. Replay does not grant a new source or structural
header allowance. Its returned header_bytes describes the same
recognized bytes already counted by traversal, not additional message
headers.

The cursor composes first-valid metadata selection, bounded lowercase
head copying, selected protocol charset and normalized filename
precedence. Backing supplies separate fixed heads, charset and filename
windows. Heads holds the complete lowercase type/subtype followed by the
optional lowercase disposition without a separator; View slices
distinguish them. Charset spelling and its Absent/Invalid/Present,
known-label and qualifier diagnostics are retained. A default type has
no selected charset field and therefore no charset_end; a selected type
without a charset has an explicit Absent end. Unknown valid labels
remain Present with no known_charset, never a substituted default. These
are selected-field evidence, not the final JMAP charset property;
POLICY.md's implicit us-ascii rule and RFC 8621 section 4.1.4's
non-text null charset property mapping remain downstream. Filename preserves disposition filename before type name,
selected-empty presence, NFC and the existing independent
decoding/family diagnostics.

No bytes are exposed before all phases complete. check_deadline and
consuming finish require fresh original admission. finish returns the
passive View and original Meter/HeaderBudget/Scratch; subsequent
metadata can spend these same owners while backing remains borrowed.
Cached Complete is inert. All child work/interpretation refusals map to
Admission; other errors retain their child context. Any syntax,
capacity, work or interpretation refusal retires the whole result;
neither a later duplicate nor a fallback may mask a resource failure.
Backing is not wiped on refusal/abandonment; provisional cells and
copied completed Views carry no completion, source, locator or
publication authority on their own.

This is a serialized projection after traversal releases its parser
state, not a second simultaneous traversal frame set. M06cv adds passive
CID/language field retention as described in 1.101. Integration of traversal,
retained metadata and body lists into JSON, location, authenticated locators,
streaming and worker/native stack/RSS qualification remain open.
RESOURCES.md qualifies its fixed state and Rust allocation checks
separately.

### 1.87 Iterative resident body lists

M06ch adds mime_body_lists. Class::from_headers classifies one healthy
retained part-header View with fixed comparisons under original
Meter/HeaderBudget. It expects canonical lowercase heads from the preceding
cursor, preserves attachment/inline distinction and treats a selected empty
name as unnamed only for body selection. Its generated Class is passive
data. Caller Node slots supply that classification plus completed traversal
parent/depth; implicit ordinals are slot indices plus one. The caller pins
correspondence, complete source authorization and original retention
admission, including matching Node parent/depth and multipart classification
to each traversal Part.

Cursor validates the configured depth/part limits and preorder shape while
building text/html ordinal lists in separate fixed caller windows. Its 65
frames include a virtual mixed scope, supporting 64 actual entity levels.
One turn enters a node, closes one frame, copies one fallback ordinal or
sweeps one attachment. Multipart containers recurse through frames
regardless of their own disposition; attached message types remain leaves.
Related permits an inline leaf only at its first immediate-child position,
regardless of name; all later leaf children become attachments. In other
multipart scopes, later named text is an attachment, while empty names
remain eligible. Alternative selection uses scope-local channel masks and
bounded fallback copying, including POLICY.md's explicit disabled-channel
rule. A final preorder sweep derives attachments from both RFC membership
conditions, never duplicate provisional append decisions. has_attachment
requires an attachment whose disposition is not Inline.

Lists and has_attachment appear only after complete success. Capacity,
malformed tree, work/interpretation refusal and late deadlines are sticky;
backing is not wiped and carries no completion/publication authority. Cached
Complete is inert. check_deadline and consuming finish freshly check the
original owners; finish returns passive slices and those same owners. No
header bytes/steps are renewed or consumed by typed tree/list work; all
node, membership and fallback visits, structural turns and writes spend
original job allowances. HeaderBudget remains exclusively borrowed and its
existing sticky refusal still retires completion.

RESOURCES.md qualifies state, turns and Rust allocations. Nodes/lists are
caller-supplied evidence without partId/blobId or wire authorization.
Retained list windows need separate aggregate output admission; no body-work
scratch partition is claimed for them. Automatic traversal/metadata
coordination, JSON/body values, remaining part headers, authenticated
locators and streaming/worker/native/RSS remain open.

### 1.88 Shared checked resident extents

M06ci moves absolute resident slice mapping into td-header::resident::slice.
Header selection/value composition, MIME metadata, selected part headers and
traversal use it atomically, preserving their prior
InvalidState/InvalidRange mapping and original source/budget ownership. It
checks half-open selected extents, including base subtraction and usize
conversion, without inspecting or copying bytes. Any empty in-window range
is valid; before-base, reversed and out-of-window extents fail. The returned
view is passive and carries no lexical, source or publication proof. Callers
fund each subsequent read and retain complete-source admission. This adds no
cursor, work allowance, memory reservation or dependency.

### 1.89 Budgeted Content-Language values

M06cj adds mime_language::Cursor over one caller-authorized complete resident
field value. The std-only td-header::language_list engine composes existing
CFWS and passive tag spelling; tags preserve original case, order and
duplicates. At least one comma-separated tag is required. CFWS is outside
contiguous tags; malformed/empty tails and excess comment depth reject the
whole value. This is field-value syntax, not field discovery, selected-header
metadata, locale/charset inference or new header-form dispatcher policy.

Every returned Tag extent is provisional and must be retired if later syntax
or original admission fails. No output backing or wire/publication proof is
created. Caller retention remains separately admitted. The non-Copy/non-Clone
mail cursor exclusively borrows the original Meter/HeaderBudget and private
credit, funding source/EOF visits and transitions before access. No copied
or replacement allowance can resume it. Cached Complete is inert; is_complete
is passive. check_deadline and consuming finish freshly admit original owners;
a late refusal retires completion and all earlier events. Healthy finish
returns those same owners without refund or renewal. finish checks admission
before completeness: an expired premature finish returns Work(Deadline),
while a healthy premature finish returns InvalidState.

RESOURCES.md qualifies fixed state/turns and Rust allocation. Automatic
part-header selection/retention, body JSON/value projection, authenticated
locators and streaming/worker/native/RSS remain open.

### 1.90 Budgeted Content-ID values

M06ck adds mime_content_id::Cursor over one caller-authorized complete
resident field value. Private purposes reuse header_message_ids syntax and
project::Budgeted validation/replay, unfolding and scalar conversion. Exactly
one identifier is required; the public MessageIds constructors still parse
lists. The adapter maps every inner error to a field-specific Error enum
and Content-ID diagnostic. Full-value syntax or nesting refusal precedes every Begin/Scalar/End
event. Later admission failure retires all provisional output. The adapter
omits outer angle brackets and CFWS between tokens, preserving quoted/escaped
spelling and the existing Unicode diagnostic policy without NFC or encoded
word decoding. Intermediate unfolding bytes and final scalar UTF-8 bytes
both spend original output work; this helper emits no JSON.

The non-Copy/non-Clone cursor keeps the original Meter/HeaderBudget and
private credit. Each bounded child turn uses the existing conversion limits.
Cached Complete is inert; is_complete is passive and is_encoding_problem
returns a diagnostic only while completion remains healthy. check_deadline
and consuming finish freshly admit original owners; late refusal hides
completion and the diagnostic. finish checks admission before completeness,
returns the same owners on success and gives InvalidState for a healthy
premature handoff. No allowance is refunded or renewed.

Selection, retained backing, body metadata/JSON, CID reference resolution,
authenticated locators and streaming/worker/native/RSS remain open.

### 1.91 Shared URI syntax with a required scheme

M06cl moves header_urls' private URI validator to td_header::uri::Validator.
The existing header_urls and Budgeted APIs keep their URL-list and ListPost
semantics, validation before output, original allowances, diagnostics and
charges. Mail binds shared work to the same original decode-work adapter and
maps shared malformed/work/invariant errors back to its existing variants.
IPv6 closure still prepays 64 records before its fixed local parse. No
relative URI, Content-Location value or scheme-specific policy is added.

The shared validator owns only fixed syntax and a bounded IPv6 byte array;
its work/syntax refusal is sticky, and cached finish performs no work. Mail
continues funding source reads and parent transitions before feed, binding
fresh final admission and retiring the enclosing owner after refusal. The
shared spelling result is passive and grants no I/O/publication authority.
RESOURCES.md scopes shared state and consumer accounting qualification.

### 1.92 Shared URI-reference syntax

M06cm adds td_header::uri::Validator::reference for RFC 3986 URI-reference
spelling, including empty references, relative paths, network paths and
query/fragment-only references. A colon in the first segment requires a
valid scheme prefix; percent spelling cannot manufacture a scheme. Paths
preserve dots and delimiters without base resolution or normalization.
The existing new constructor still requires a scheme, so public mail URL
forms keep their prior grammar and accounting.

Reference state shares fixed percent, host/port, IPv6/IPvFuture validation
and sticky work/syntax refusal. Feed/EOF admission and enclosing source
identity remain with the caller. Cached syntax completion and fresh
zero-count check_work retain their original contracts. Callers decide
whether empty references or particular schemes are valid in a field.
Content-Location CFWS/folding/encoded-word decoding, first-valid part
selection, retained JSON and worker/native/RSS remain separate.

### 1.93 Shared URI wire unfolding

M06cn adds td_header::uri::unfold::Cursor over a caller-selected immutable
URI wire spelling. Exclude surrounding CFWS and the final header ending.
Remove SP/HTAB and folded CRLF or bare LF followed by WSP, preserving all
other octets and offsets within the supplied spelling slice; the caller
rebases them to field/message positions. Nonfold CR/LF and unfinished folds
fail. Percent spelling, comments, quotes, encoded words, NUL and 8-bit
octets are literal here: unfolding supplies no URI/charset validation or
encoded-word placement authority.

One poll admits one source octet or EOF before access, with at most one
visit and one record; whitespace runs yield without an unbounded scan.
Octets are provisional until complete unfolding, and all retire on
later syntax/work/enclosing refusal. Cached Complete is inert; check_work
freshly admits zero-count work and retires completion on refusal, sticky
across replacement callbacks. Live state is neither Copy nor Clone and
owns no work, clock, output buffer or growing collection. The caller retains
its original source/job/header allowances and funds retained output.

This shared preprocessing step supports RFC 2557 section 4.4's unfolding
before encoded-word decoding and RFC 2017 section 3.1's wire-whitespace
removal. It does not implement Content-Location CFWS, complete URI labels,
encoded-word decoding or first-valid metadata selection. Existing mail URL
forms retain their grammar and work. Worker/native/RSS remain open.

### 1.94 Private payload-free encoded-word progress

M06co separates private transfer/charset progress from the public borrowed
encoded_word::decode::Cursor. The public cursor retains its recognized Word
and delegates; signatures, scalar filtering, repair diagnostics, cached
completion, sticky refusal and exact work remain unchanged. No field or
encoded-word placement policy changes.

Private Progress retains no payload borrow, clock or allowance. Its enclosing
owner supplies the same recognized immutable logical word on every turn,
allowing fixed scratch bytes to relocate without self references. Word
recognition and placement stay separately admitted under original budgets.
An admitted turn rejects changed charset, transfer encoding or payload length
as InvalidState before payload access. Those checks do not bind byte content:
the enclosing owner must keep logical bytes immutable through completion.
Work/deadline refusal precedes shape checking and remains sticky across fresh
callbacks. Cached Complete is inert; final admission belongs to the original
live enclosing owner. Existing source-bound checkpoints may copy pure progress
without copying or replacing admission. No public source-free decoder or
Content-Location composition is introduced.

### 1.95 Selected URI encoded-word reader

M06cp adds mime_location_word::Cursor over one caller-selected and
placement-authorized URI encoded-word wire spelling, excluding surrounding
CFWS and the final header line ending. It removes wire whitespace through
the shared URI unfold cursor before recognizing a complete encoded word.
Complete fold validation precedes every scalar; malformed tails fail even
when the logical candidate exceeds the 75-octet fixed scratch buffer.
Unknown charset, invalid word syntax or oversized candidates complete with
End.recognized false and no scalars. The enclosing owner replays that whole
literal token; this reader supplies no prefix fallback or URI validation.

Recognized logical bytes stay immutable in the owned fixed scratch through
decoding. Private relative Descriptor metadata reconstructs borrowed Word
views after cursor moves, preserving language metadata without source
pointers, byte scans or repeated recognition. The cursor funds recognition
and decoding through its original job/header owners. The caller separately
funds placement and source selection.
Descriptor reconstruction grants no admission and does not bind byte content.

Cursor is neither Copy nor Clone and retains the original job Meter and
aggregate HeaderBudget. Scalars have separately charged UTF-8 output and
remain provisional until Complete and fresh original admission. end returns
recognition and repair diagnostics only at healthy completion. Cached
Complete is inert; check_deadline retires even completed results on refusal.
Consuming finish(now) checks fresh admission and returns the original owners
and End only after healthy completion. No NFC, CFWS selection, word placement,
whole Content-Location parsing, label resolution or publication is provided.

### 1.96 Selected literal URI-reference reader

M06cq adds mime_location_literal::Cursor over one caller-authorized selected
URI spelling outside surrounding CFWS and the final header line ending.
It unfolds wire whitespace and validates the entire RFC 3986 URI reference
before replaying any literal ASCII octet. Empty relative references remain
valid at this layer; whole-field presence and grammar are external. Syntax
or fold errors emit no octets. The caller separately establishes encoded-word
placement; this literal path leaves word markers, percent spelling, case,
path segments and parentheses unchanged. No NFC or resolution occurs.

Octet events retain offsets within the supplied spelling slice, which the
caller rebases into the authorized field/message. Replay uses the same
immutable source and original job/header owners. Each visit, bounded IPv6
parse and projected output byte is funded. Events remain provisional until
healthy Complete and fresh original admission. Cached Complete is inert;
check_deadline retires completion on refusal. Consuming finish(now) performs
fresh admission and returns the original owners only after completion.
Admission precedes premature-finish InvalidState; expiration retains work
refusal precedence. Cursor is neither Copy nor Clone and adds no source/publication authority,
whole Content-Location parser or retained metadata policy.

### 1.97 Shared URI spelling selection

M06cr adds td_header::uri::spelling::Cursor for one complete immutable
field-value slice under the caller's explicit surrounding-CFWS permission.
POLICY.md owns the ambiguous parentheses rule: greedy leading CFWS and a
complete terminal CFWS suffix beginning with WSP or a fold are preferred;
after leading CFWS, adjoining parentheses and failed optional suffix grammar
remain literal. Leading malformed/over-nested comments reject selection;
Work/InvalidState never enter grammar fallback. Source-relative start/end
offsets can be empty and grant no whole-field presence, URI/fold validity or
word placement.

Each poll invokes one funded bounded CFWS turn or one funded source/probe
step. The cursor borrows source, retains fixed exclusive child state and owns
no work/deadline/output owner. It is neither Copy nor Clone. Cached Complete
is inert; check_work uses fresh zero-count admission and retires success on
refusal across replacement callbacks. Consuming finish requires healthy
completion but no new admission. Mail's eventual composer must keep original
job/header owners, freshly admit before retaining the range and rebase offsets
to its authorized field/message. This does not activate a complete
Content-Location reader or metadata/publication authority.

### 1.98 Original-owner URI spelling selection

M06cs adds mime_location_selection::Cursor around the shared spelling
selector. Caller supplies one complete immutable field-value slice excluding
its final ending and authorizes POLICY.md's surrounding-CFWS preference.
It returns source-relative Spelling offsets, including empty ranges, without
proving URI/fold validity, encoded-word placement, field presence, duplicate
selection or metadata/publication authority. Invalid URI bytes can complete
this boundary phase; later readers validate their selected spelling.

The live cursor retains exactly the original job Meter and HeaderBudget,
charging every shared visit/record and replay through those owners. A live
poll checks fresh original admission before parsing; cached Complete is
inert. check_deadline retires completed offsets after refusal. Consuming
finish(now) freshly admits before returning both original owners and the
range. Admission refusal precedes premature-state failure. Consuming refusal
drops this cursor's borrows; enclosing caller scopes retain ownership of
the root Meter/HeaderBudget allocations. The error does not carry new
allowances or recoverable cursor progress. No growing value
copy or replacement allowance is introduced. The cursor is neither Copy nor
Clone. A future composer must keep the returned owners for subsequent
placement, decoding and retention; this is not a complete Content-Location
reader.

### 1.99 Literal URI field-value pipeline

M06ct adds mime_location_literal_field::Cursor for a complete immutable
field-value slice excluding its final ending. Caller explicitly authorizes
POLICY.md's surrounding CFWS and selects literal interpretation. The cursor
selects boundaries, validates the complete unfolded URI reference, then
replays literal ASCII with offsets rebased into that original field-value
slice. No octet precedes full CFWS selection and URI/fold validation.
Word-looking bytes, percent spelling, case and parentheses remain literal;
no NFC, resolution or fetching occurs. Empty URI references remain permitted.
Encoded-word placement/path choice and field presence remain external; this
literal path does not activate a complete Content-Location parser. Interior
comment-looking runs remain URI data under the selected boundary policy;
wire whitespace is removed, so `a (b) c` projects as `a(b)c`.

Exclusive phase state owns either the selection cursor or the literal
reader, transferring the same original Meter/HeaderBudget through fresh
consuming handoff. It retains only source and passive range metadata, without
self references or a selected-value copy. Unused prepaid credit is not
exported at handoff; the next phase charges conservatively through those
same owners. One poll invokes one bounded child turn plus fixed metadata
checks. Octets remain provisional until healthy Complete and fresh original
admission. Cached Complete is inert; check_deadline retires completion on
refusal. Consuming finish(now) freshly admits and returns both original
owners plus Spelling after complete validation/projection. Fresh admission
refusal precedes premature-finish InvalidState. Failure drops
borrowed progress; root allocation ownership remains in the enclosing caller.
No retry grant, retained metadata or source/publication authority follows.
Live state is neither Copy nor Clone. Caller separately maps these offsets
into its authorized resident/message extents.

### 1.100 Resident MIME label field selection

M06cu adds mime_label_fields::Cursor over caller-authorized resident entity
bytes and their absolute base, header limit and explicit Eof/Prefix ending.
Reuse the raw header scanner and strict single Content-ID and complete
Content-Language grammars. Select the first completely valid occurrence of
each field, skipping malformed occurrences and scanning later duplicates
without interpreting their values. Names match ASCII case insensitively,
including scanner-supported obsolete whitespace before the colon. Missing
or entirely malformed values select None independently for the two fields.

One exclusive syntax child is live at a time. Every scan, name comparison,
parse and EOF decision uses the same original Meter/HeaderBudget and shared
prepaid credit. Syntax fallback discards only candidate progress; it does
not reset work or aggregate admission. Nesting, raw-header bounds, truncated
prefixes, work and interpretation refusals retire the entire selection.
A Prefix can succeed only when the scanner establishes a complete header
boundary inside the supplied bytes; Eof additionally permits true EOF.

No selected fields are visible before complete header-section success.
Cached selection is provisional through fresh original admission; cached
Complete polls are inert. check_deadline retires cached results on refusal.
Consuming finish(now) freshly admits before premature-state failure and
returns the exact original owners and passive Selection, including absolute
raw field extents and scanner End. The caller separately projects/retains
CID strings and ordered language tags under returned owners. These extents
grant no raw source, blob, uniqueness, reference or publication authority.
Cursor is neither Copy nor Clone. Location, JSON and traversal coordination
remain separate; no growing label list or value copy is allocated.

### 1.101 Retained CID and language fields in part headers

M06cv extends mime_part_headers::View with content_id_field and
content_language_field: optional passive absolute mime_headers::Field
extents. After filename completion, the part-header cursor runs
mime_label_fields::Cursor over the same complete authorized entity,
original header limit and original Meter/HeaderBudget. Scratch remains
borrowed without use by this child. The second scan spends original
interpretation work; it grants no additional raw-header allowance. The
label selector's body boundary and recognized raw-header count must match
initial metadata selection before any View becomes visible.

First-valid complete CID and language selection follows 1.100 and POLICY.md.
Missing or wholly malformed fields retain None independently. Values remain
raw extents: case, folding, comments and UTF-8 bytes are preserved. This adds
no label backing window, CID string, retained tag list or MIME part-cell
format. Consuming finish freshly admits and returns the passive View plus
pointer-identical original Meter/HeaderBudget/Scratch. The caller may fund
CID or language projection with those owners and its same authorized source.
Extents grant no source, blob, uniqueness, reference or publication authority.

A label child refusal retires the whole part-header result, including
previously completed heads, charset and filename. Work/interpretation errors
map to Admission; structural/nesting errors retain Error::Labels context.
Mapping these helper refusals into final JMAP BodyPart/null/error responses
remains downstream and unavailable in this increment. Caller backing is not
wiped and remains provisional after failure. Cached
Complete remains inert; check_deadline and finish require fresh admission.
Location dispatch, automatic traversal integration and JSON projection remain
separate increments.

### 1.102 Selected Content-ID JSON string

M06cw adds mime_content_id::json::Cursor for one caller-selected complete
Content-ID field value. It binds the existing CID projector to shared
std-only td_json::string::Frame for the whole cursor lifetime and borrows
original Meter/HeaderBudget. Constructor admission is inert. Input and
selected-field authorization remain caller responsibilities; absent or
malformed-field null mapping belongs to the selecting coordinator.

poll(now, output) emits provisional serialized bytes through Progress
(written, Yield/NeedOutput/Complete). Short output windows preserve paid
pending bytes; empty output performs fresh admission without visiting input.
A cached Complete is inert. Existing whole-value syntax validation precedes
CID scalar events, but the opening JSON quote may already be provisional.
Every syntax or work refusal retires the whole JSON string, including that
quote and any previous fragments. A source cannot be replaced or rewrapped
inside this cursor. No source grant, renewed interpretation/output allowance,
retained string, publication transaction or locator authority follows.

CID case, quoted spelling, escapes and decomposed Unicode follow 1.90;
encoded-word-looking identifiers stay literal and NFC is not applied. The
existing conversion repairs disallowed noncharacters and reports its
is_encoding_problem diagnostic. end() exposes passive End only after JSON
completion; a failed fresh check clears it. Serialized quotes, escaping and
UTF-8 use the shared framer's ordinary JSON policy and exact serialized output
charges, in addition to the original CID conversion charges.

check_deadline and consuming finish(now) freshly admit even after Complete.
finish checks admission before premature-state failure and returns original
Meter/HeaderBudget plus End. Partial output remains caller-reserved and is
not wiped after refusal. The fixed source/framer pair is neither Copy nor
Clone; this helper composes no part tree, null property or whole response.

### 1.103 Shared string-array framing

M06cx atomically moves MessageIds and URLs array punctuation and nested
string framing into std-only td_json::string_array. The mail coordinator
still validates the whole field before emitting array bytes, selects the
existing field mode, maps whole-field malformed values to null, and owns
original Meter/HeaderBudget handoff. The shared frame retains no values or
allowances. Array and String callback roles preserve existing typed error
contexts for punctuation and nested scalar/escaping work.

Balanced Begin/Scalar*/End events delimit each string; Complete ends the
array. Short drains retain already-paid bytes. Empty output freshly admits
without advancing input; cached completion is inert. Any protocol/source
or admission failure retires all provisional output. Fresh consuming mail
handoff requires complete drained array/null framing and the original
converter outcome. Public forms, literal output, diagnostics and existing
resource ceilings remain unchanged. This primitive introduces no field,
source, locator or publication authority; Content-Language JSON remains a
separate binding.

### 1.104 Selected Content-Language JSON array

M06cy adds mime_language::json::Cursor over one caller-selected complete
Content-Language field value and the original Meter/HeaderBudget. Constructor
admission is inert. Shared std-only string-array framing serializes literal
tag spelling, preserving order, case and duplicates. No locale lookup,
normalization, encoded-word decoding or retained tag list is introduced.
Caller source/field authorization, absence/null mapping and publication
remain separate obligations.

Each grammar Tag extent begins one JSON string. Its ASCII spelling replays
from the same immutable authorized source with one original source visit and
interpretation step per byte, sharing the grammar's prepaid record credit.
Quotes, commas, brackets and tag bytes spend exact serialized output charges.
The cursor retains one current extent/position and one shared array frame;
no source-sized buffer, fresh allowance or tag-count-sized collection follows.
Each poll runs one grammar turn, one replay byte or one bounded framing turn.

poll(now, output) returns Progress with Yield/NeedOutput/Complete. Empty output
freshly admits without advancing source; short drains preserve paid bytes.
Individual tags and all array bytes remain provisional until whole-list
syntax and array framing both complete. A malformed tail can therefore retire
already serialized tags. Syntax/nesting/resource/admission refusals latch
the original cause and hide completion; all partial bytes retire. Missing or
malformed fields do not become null automatically in this selected binding.

is_complete requires both the healthy shared frame and complete grammar.
Cached Complete is inert; check_deadline and consuming finish(now) freshly
admit even after Complete. Finish admits before premature-state refusal,
requires the complete drained array and returns the exact original owners.
Premature finish uses shared Error::InvalidState(Role::Array); Role is
reexported alongside Progress/Status. Source causes remain typed language
errors, including child state, grammar and admission refusal.
The caller may reuse them for another projection without renewed grants.
Cursor is neither Copy nor Clone. No source, locator, retained-part or
whole-response publication authority follows.

### 1.105 Retained selected MIME label JSON

M06cz adds mime_label_fields::json::Cursor. Values supplies optional
authorized complete selected CID and language values; Backing supplies
separate mutable windows reserved by the caller. The constructor is
inert. Field discovery, source extent authorization, optional-field/null
policy and response publication stay with the enclosing owner. A copied
field extent does not authorize its source.

The cursor runs the existing selected CID and language JSON bindings in
order, with the original Meter/HeaderBudget held exclusively by the
collector or one active child. Drained paid JSON bytes go directly into
backing without another output charge or an additional staging buffer.
Child handoff may forfeit unused prepaid record credit; it never renews
credit or grants. No retained tag list or growing buffer follows.
Missing values remain absent and use no output window. Selected syntax,
nesting, work, interpretation, admission or capacity refusal retires the
whole pair, including an already completed earlier fragment. Partial
backing is never a successful Retained result and is not wiped on
failure.

poll(now) yields Yield or Complete. Cached healthy Complete is inert.
Fresh check_deadline and consuming finish(now) admit the original
owners, even after Complete and before premature-state refusal. A
complete healthy finish returns Retained plus the pointer-identical
original owners. Retained contains optional complete JSON fragments and
optional final CID diagnostics; it is passive borrowed data, without
blob, source or publication authority. Absence is not serialized as null
here. Capacity cannot expand and must cover the entire serialized
fragment. Checked content_id_capacity_bound(raw_bytes) and
content_language_capacity_bound(raw_bytes) return conservative bounds
6N+2 and 2N+3, or None on usize overflow. N is the complete raw
field-value length, including CFWS. They grant neither validity nor
output work. Missing values need no window. Cursor is neither Copy nor
Clone.

Refusal shapes retain phase context: transitions/final admission use
Admission(nfc::Error), while active children use
ContentId/ContentLanguage with their typed source causes. Fresh
admission precedes capacity checking; a full window may refuse capacity
before selected syntax can be inspected. The inert constructor and at
most two transition-only polls add no parsing or output charge. A
completing child poll also performs fresh consuming handoff and owner
restore with no additional resource charge.

### 1.106 Composed part headers and selected label JSON

M06da adds mime_part_headers::label_json::Cursor. Its Entity is the same
complete authorized immutable entity and absolute base as the existing
part-header cursor. Backing supplies the existing independent heads,
charset and filename windows plus separate CID/language JSON windows.
M06dh also requires its distinct content_location JSON window.
The existing header owner first selects metadata and first-valid label
fields, then hands its original Meter, HeaderBudget and Scratch to the
exclusive label phase. Selected extents are checked against the
recognized header section and mapped only into that same Entity source.
This passive mapping neither reads bytes nor grants source
authorization. Body bytes cannot supply a selected header; no blob or
response authority follows.

value() returns no metadata before whole healthy composition completes.
Its passive View combines the existing header View, complete retained
label pair including final CID diagnostics, and M06dh's source-bound
location fragment/selection. Missing labels remain absent; this layer
does not serialize null. All original
selection, normalization, syntax, capacity and resource rules remain
delegated to the existing children. Any child or mapping refusal retires
the entire composition and hides all three groups, including already
completed metadata. Provisional caller backing is not wiped and must not be
published after refusal.

poll(now) invokes at most one active child poll; a completing child is
freshly consumed in that turn and the same original owners are handed to
the next phase or restored. Those handoffs add no parsing/output charge
and never renew prepaid credit. Header, label and location cursors never
coexist as live owners. Errors retain Headers, Labels or Location context
during the active child and its consuming handoff. Fresh admission after
healthy completion uses Admission; invalid extent/state uses InvalidState.
Cached healthy Complete is inert. Fresh check_deadline and consuming
finish(now) admit the original owners even after Complete and before a
premature-state refusal. Healthy complete finish returns View with the
pointer-identical original Meter, HeaderBudget and Scratch, reusable for
subsequent interpretation. Cursor is neither Copy nor Clone. Before
parsing, reserve windows using the existing head/filename bounds and
selected-label JSON bounds plus the checked
mime_location_field::retained::capacity_bound(N) for its independent
Content-Location window, with an admitted upper bound on recognized
header bytes as N. Each selected raw field value fits within that bound;
its exact length is not needed to reserve backing. No window grows here.
Traversal, location placement, source authorization and response
publication remain separate.

### 1.107 Selected URI encoded-word runs

M06db adds mime_location_word::Cursor::new_run for a complete
caller-selected, placement-authorized encoded-word run, excluding
surrounding CFWS and the final header ending. Constructor admission and
single-word new behavior are unchanged. Complete wire unfolding precedes
a whole-run recognition pass; only then does a second candidate pass
decode scalars. Wire whitespace is removed before recognition. Adjacent
words require a gap containing removed wire whitespace in the original
source; touching words request whole fallback. Each logical word has the
shared 75-octet ceiling; a run may be longer without growing scratch.

Unknown charset, invalid word syntax, oversized words, empty spelling or
mixed literal text complete with End.recognized false and no scalars.
The enclosing owner replays that entire selected spelling. Malformed
folds and original resource refusal are fatal, including after an
invalid candidate. Classification deliberately funds the remaining
source through admitted EOF after rejection; the prior fold pass does
not prepay or bypass this complete classification attempt. Healthy runs
preserve decoded spaces, non-ASCII, case and decomposed spelling. No URI
validation, percent decoding, NFC, field placement or label resolution
follows. End.encoding_problem combines every word's repair diagnostic.

The original Meter, HeaderBudget and prepaid credit fund all three wire
passes, both bounded recognitions per word, decoding and scalar output.
One fixed buffer is immutable during each word's decoding; relative
descriptors survive cursor relocation. Scalars remain provisional until
whole healthy completion and fresh original admission. Cached Complete,
sticky refusal, final admission and pointer-identical owner handoff
retain the single-word contract.

### 1.108 Complete authorized URI field projection

M06dc adds mime_location_field::Cursor over one complete caller-authorized
field-value slice, excluding the final header ending. The caller authorizes
surrounding CFWS and whole-spelling encoded-word placement. Selection uses
the existing ambiguity policy and returns offsets into that same source.
Only a complete recognized word run decodes; unknown, oversized, touching
or mixed spellings replay the whole selected spelling through the existing
literal URI-reference validator and projector. No known prefix is decoded
before fallback. Decoded labels preserve spaces, non-ASCII and decomposed
spelling without a later URI check, percent decoding or normalization.
Empty references remain permitted; field discovery and presence policy are
external. No label matching, URL access or publication authority follows.

Status::Scalar contains a provisional Unicode scalar; literal ASCII uses
the child's already-paid output, without a second conversion charge. End
contains the selected Spelling, encoded_words and combined encoding_problem.
No End escapes before whole healthy completion. Selection, Words and Literal
errors retain child context; fresh admission after completion uses Admission.
Context identifies the phase that failed, not the eventual field form: fold
errors come from Words even for a plain literal spelling.
Any refusal retires its parser and hides every metadata extent/End. Only
malformed source syntax retains the original owner references for private,
freshly admitted consuming discard; all other refusals retire those owners.
Provisional caller output is not wiped. Cached Complete is inert, while
check_deadline and consuming finish(now) freshly admit even Complete before
premature-state refusal.

One poll invokes at most one active child poll. Fresh consuming handoffs
move the original exclusive Meter and HeaderBudget to the next child or
restore them at completion, adding no charge or renewed credit. Healthy
finish returns the pointer-identical reusable owners and End. Source ranges
are checked before child construction, and the cursor is neither Copy nor
Clone. Retained strings/JSON, duplicate-field selection, MIME traversal and
response publication remain separate composition.

### 1.109 Complete location JSON string projection

M06dd adds mime_location_field::json::Cursor, binding shared td-json string
framing to the complete authorized field cursor and its original owners.
The constructor carries the same CFWS, placement, source-ending and presence
contracts. Decoded labels and literal fallback preserve the scalar cursor's
spelling, repair diagnostics and character policy, including encoded-control
dropping. No additional URI check, percent decoding or NFC occurs.

poll(now, output) returns shared Progress/Status, copying at most six paid
wire bytes or invoking at most one field poll per turn. Empty output performs
fresh admission only and returns NeedOutput before healthy completion. JSON
quotes, escapes and UTF-8 consume their exact serialized lengths in addition
to the field's already-paid scalar output. These framing charges use the
original Meter and fresh header admission without adding source visits,
interpretation steps or prepaid credit. Errors wrap the active field phase;
opening charges use Selection, scalar stages use Words/Literal, and closing
or completed admission uses Admission. Framing InvalidState stays separate.

End is the scalar cursor's passive spelling/form/repair metadata and remains
hidden until the whole JSON string drains. Every refusal hides it and retires
both source and frame; already-emitted bytes are provisional and not wiped.
Cached Complete is inert. check_deadline and consuming finish(now) freshly
admit even Complete; healthy finish returns the pointer-identical original
Meter, HeaderBudget and End. Source and frame identity stay bound across
short borrows and moves; the cursor is neither Copy nor Clone. Missing-field
null selection, retained output windows, response ownership and publication
remain enclosing obligations. A malformed field can follow a provisional
opening quote; it never publishes a successful JSON string.

### 1.110 Retained optional selected location JSON

M06de adds mime_location_field::retained::Cursor over an optional complete
caller-authorized selected field and one distinct caller-reserved backing
window. Missing input remains absent; a present empty reference retains the
empty JSON string. Field discovery, first-valid duplicate choice, null
mapping, URL/label matching and publication remain external. capacity_bound
uses the shared checked string sizing helper: URI conversion and escaping
need at most six serialized bytes per original field octet, plus two quotes.
The helper grants no source, storage reservation or output allowance.

The cursor keeps one bound JSON child and shared td-json::retain::Window.
Every retained turn performs fresh original admission and at most one child
poll. Construction and consuming handoff add no charge or renewed credit;
all source, interpretation, scalar and serialized output costs remain those
of the independent public JSON child. A child completing its closing drain
is freshly consumed in that turn to restore the same original owners.
Capacity and projection failures retire the whole value and hide its View;
provisional backing bytes are not wiped. Empty/full backing never triggers
a renewed window or output quota. Missing input needs no backing or output.

view() returns passive Retained { value, end } only after whole healthy
completion; field provenance/form/repair and JSON bytes become visible
together. Every later refusal hides the view. Cached Complete is inert;
check_deadline and consuming finish(now) freshly admit even Complete before
premature-state refusal. Healthy finish returns Retained borrowing the same
reserved backing and the pointer-identical reusable Meter/HeaderBudget.
The cursor is neither Copy nor Clone and supports relocation between polls.
A view remains provisional through whole-job final publication admission.

CID/language pair retention migrates atomically to the same shared window
primitive, retaining its public Backing/Values/Retained APIs, error contexts,
first-valid selected sources, exclusive children and original cost contracts.
The shared window tracks only fixed backing and checked reported prefixes;
it grants no source identity, JSON validity, completion or publication.

### 1.111 First-valid resident Content-Location fields

M06df adds mime_location_fields::Cursor using the existing caller-authorized
resident Input contract, raw Scanner, shared field matching/range helpers and
complete location projection. Select the first completely valid field,
including an empty reference; skip only malformed needed occurrences. Later
duplicates are scanned without interpreting their values. A raw scanner
boundary (including its existing non-header body rule) must complete before
selection() exposes any passive field extent, form or repair diagnostic.
Raw field values and absolute offsets preserve the original resident source;
no JSON retention, URL matching, normalization or publication grant follows.

One inline field child exclusively owns the original Meter/HeaderBudget.
Its provisional scalars are discarded, but their original output charges
remain funded. Healthy child finish returns original owners; a malformed
source discards the dead parser and retains only those same owner references.
An internal consuming discard freshly admits original allowances before
another occurrence can be tried. No credit, budget or source is renewed.
Errors preserve their phase: Headers carries scanner refusals, Location
carries the active field child's refusal, and Admission carries the outer
original-budget check. These contexts are fatal except for the explicitly
consumed malformed field syntax; no resource refusal becomes absence.

Nesting, work, interpretation, invalid-state and raw-header refusals never
return this malformed-discard path and retire the whole selection. All
cached public field failures remain sticky; no failed field becomes valid.

Every live turn freshly admits and polls at most one scanner or field child.
The complete selection remains provisional through fresh consuming finish,
which returns the same pointer-identical reusable Meter/HeaderBudget and
passive Selection { content_location, location_end, end }. Later refusals
hide the whole selection. Cached Complete is inert; check_deadline and
finish freshly admit completed owners before premature-state refusal.
The cursor is neither Copy nor Clone and uses fixed inline storage. Retained
location projection must still use the original selected source and original
allowances; M06dg provides that bound composition.

### 1.112 Source-bound retained Content-Location

M06dg adds mime_location_fields::json::Cursor binding one immutable
caller-authorized resident Input, original Meter/HeaderBudget and distinct
reserved JSON backing. Complete first-valid discovery precedes optional
retention. Slice only the selected extent from that same original Input;
no caller can substitute another source between those phases. The handoff
freshly consumes the selector and polls no second child in that turn.
Retention freshly consumes its child and checks that presence and field
form/repair diagnostics agree with discovery before exposing a whole view.

Retained { selection, value } preserves raw header End and selected extent,
and absent versus present-empty JSON. Scalars fund discovery and replay
separately; JSON wire output is charged additionally. Every live turn
freshly admits original owners and polls at most one exclusive child.
Capacity, scanner, syntax, work and deadline refusals hide the whole view,
including selection metadata, and stay sticky. Cached Complete is inert;
explicit check and consuming finish freshly admit before completed or
premature-state decisions. Finish returns the same reusable original
owners and a borrowed fragment in caller-reserved backing. The cursor is
neither Copy nor Clone. The view remains provisional through enclosing
publication; URI resolution, label matching and response/null policy stay
external.

### 1.113 Complete part metadata with retained location

M06dh atomically extends mime_part_headers::label_json::Backing/View and
all callers with content_location reserved backing and location retained
metadata. Preserve the original complete Entity through the existing
header and CID/language phases, then bind M06dg's location discovery and
retention to that same entity, base, source-ending and header limit.
Scratch stays parked exclusively during location processing. Correlate
location's complete raw body_start/header_bytes with the earlier header
view before making all three metadata groups visible together.

First-valid location presence, first empty values, repair and whole-word
versus literal form remain delegated to the existing location child.
Location capacity, nesting, work or admission refusal hides completed
headers and the CID/language pair too. No source can be replaced between
phases, and no constructor/handoff charge or allowance renewal is added.
Every live turn freshly admits and polls at most one exclusive child;
whole completion is provisional through fresh consuming finish and
publication. Finish returns original Meter/HeaderBudget/Scratch and a
passive combined View. Caller-reserved bytes remain provisional on
refusal; this layer supplies no null serialization, URI resolution or
MIME traversal/response authority.

### 1.114 Traversal context for retained part metadata

M06di preserves the original MIME default context in each completed Part
without changing descriptor storage or output charging. A previously unused
flag bit now distinguishes immediate digest children independently of whether a
valid explicit Content-Type was selected. Part::context() maps that bit
to the existing metadata Context. Default cells/root context remain Normal.
DIGEST_CHILD_CONTEXT is context evidence in the diagnostics byte;
Part::problems() and PROBLEM_FLAGS expose only the five parse-problem bits.
Bits 64 and 128 remain unused. All failure, completion, fresh admission and passive evidence rules are
unchanged; copied cells confer no source binding or authorization.

Qualification hands completed descriptor extents from one original
immutable source to M06dh metadata with their recorded context, original
Meter/HeaderBudget and a separate original Scratch. Nested digest children
with missing or malformed types retain message/rfc822, an explicitly typed
child keeps its chosen type, and ordinary siblings retain text/plain.
Raw body starts correlate at zero, nonzero and maximum-fitting absolute
bases; body header lookalikes cannot supply metadata. This is component
handoff qualification. The per-entity header limit is a projection bound:
traversal already validated the aggregate recognized raw-header count.
Replay spends the same original per-email source/step and job allowances;
it does not count recognized raw headers again. Automatic whole
traversal/metadata response
composition and source/publication authority remain external.

### 1.115 Original source binding for traversal and part metadata

M06dj adds mime_response::bound::Cursor, a fixed wrapper over the
existing complete resident walker. Its fresh consuming finish creates
Structure with the same immutable source/base, admitted header bound,
completed caller descriptors and original Meter/HeaderBudget. The
binding owns those original budgets until its own fresh finish; copied
passive parts cannot construct a binding or substitute a source. Source
authorization, blob/part IDs and publication authority remain external.

Structure::metadata(ordinal, backing, scratch) reborrows only its
original job/header owners, selects a checked original descriptor and
clips only that binding's source. Original part.context() determines the
MIME default. Construct M06dh with EOF, original header bound and
independent reserved windows; no caller replacement source or new budget
enters this handoff. Scratch remains caller-owned and must be the
admitted conversion owner. Only one part cursor can borrow a binding at
a time. No second traversal frame set, retained row arena or additional
descriptor storage is added.

PartCursor freshly checks/polls at most one metadata child per turn and
correlates completed raw body_start with the traversal descriptor before
exposing PartView. Cached healthy completion is inert. Explicit
admission and consuming finish freshly admit before complete/premature
decisions. Any metadata, mapping, capacity, work, ordinal or correlation
refusal retires the binding as well as the child. The binding starts
retired with Abandoned while a part cursor exists; only successful
consuming finish restores it. Dropping or safely forgetting an
unfinished or completed but unconsumed cursor therefore leaves it
retired. Earlier copied passive evidence cannot be revoked and must be
discarded by the enclosing response owner. Structure::parts hides its
cells after refusal, and no healthy Structure finish can follow it.
Backing remains provisional rather than wiped.

Successful part finish returns passive PartView and pointer-identical
reborrowed job/header/scratch owners. After the view/reborrows expire,
the binding can project another part with separately admitted windows.
Structure::finish(now) freshly checks its original budgets and returns
passive completed descriptors with those same reusable owners. This
boundary coordinates component metadata but does not automatically visit
every part, derive body lists, serialize the response or authorize
source publication. Constructors do not parse or freshly admit; active
polls, explicit checks and consuming finishes retain that
responsibility.

### 1.116 Source-bound compact part classification

M06dk adds PartCursor::finish_classified(now). Consume the same complete
metadata with fresh original admission and body-boundary correlation
before classifying its retained headers through Class::from_headers.
Return ClassifiedView with the complete original Part (including
ordinal), retained metadata and its passive Node, plus pointer-identical
original Meter/HeaderBudget/Scratch reborrows. No caller classification,
source, parent/depth or replacement budget enters this handoff.
Successful consumption alone restores the binding; metadata,
classification or retained-node work refusal leaves it retired.

Fixed classification charges 64 source visits and 64 interpretation
steps under the original HeaderBudget, including original job I/O/work
funding. Class retention charges its size; charge the compile-time
remaining Node size under the same original output allowance before
exposing the whole node. There is no new positive cost for owner handoff
or boundary correlation. No growing state, metadata arena or additional
descriptor region is added. Caller nodes fit beside traversal parts and
membership in the existing 64-byte slot reservation. Their backing must
already be admitted.

The complete ordered owner in §1.117 enforces the correspondence that
this lower-level handoff leaves to its caller.

After serial short-borrow part consumption, fresh Structure::finish can
release the original job/header owners to the existing body-list cursor.
Caller node slots must still correspond to every original descriptor in
complete preorder. The body-list cursor checks supplied depth/parent
shape, but cannot detect swapped sibling slots or omitted trailing
parts. This method does not automatically visit all parts, enforce visit
order or uniqueness, serialize a response, derive locators or authorize
publication. Earlier copied passive metadata/nodes remain provisional
through whole response success and must be discarded after later
refusal.

### 1.117 Complete ordered original-source classification

M06dl adds mime_response::ordered. Classifying::new consumes a
healthy original Structure and clips separately admitted caller Node
backing to the completed descriptor count. Insufficient backing refuses
with NodeCapacity before another parse or positive charge. Construction
neither parses nor freshly admits. Healthy total/completed queries expose
the original descriptor count and completed prefix, so callers can stop
before an extra next request without saving a pre-construction count.

Classifying::next(backing, scratch) constructs metadata for exactly its
next original ordinal; callers cannot skip, repeat, reorder or
substitute parts. Its exclusive Part polls/checks only the original
bound child. Fresh Part::finish(now) consumes healthy original
classification, writes that node to its reserved slot and advances once.
It returns ClassifiedView with original ordinal/metadata plus the same
Meter/HeaderBudget/Scratch reborrows for one-pass response consumption.
Copies remain provisional. Node output was charged by the child;
slot/progress writes add no positive charge or renewed credit. Refusal
or safe forgetting leaves the original binding retired. An extra next
call after all parts retires it too.

Classifying::finish(now) freshly admits before the all-parts decision.
Only exact full completion creates Classified, whose private original
Structure and immutable complete nodes cannot be reconstructed from
passive copies. nodes/parts hide after explicit refusal; check_deadline
and consuming finish freshly admit. Finish returns passive complete View
plus original job/header owners. Source/blob/locator authority and
publication remain external; prior copied metadata/nodes must be
discarded after later failure. M06dm body-list binding consumes
Classified, never constructible passive View as source/order authority.
Response serialization remains next.

### 1.118 Original-source body-list selection

M06dm adds mime_response::lists. Selecting::new
consumes private Classified, separately admitted list/membership Backing,
limits and the real Tick. It freshly admits the original owners before local
limit checks, then moves them into the existing mime_body_lists::Cursor.
Only the complete original node slice can enter; there are no replacement
source, node or budget parameters. The immutable source/base and original
parts/nodes and original per-part header limit remain privately pinned.
Caller limits only constrain selection of those already bounded nodes; they
do not renew traversal or job allowances. Four Copy/Clone compile-fail guards and
one rejected passive-View constructor pin the exclusive boundary.

poll/check_deadline delegate to the existing cursor, preserving all turns,
charges, fixed frames and sticky errors. value exposes combined passive
structure and lists only after healthy whole selection. Cached Complete
remains inert and earlier copied Views stay provisional. Fresh consuming
finish returns private Selected, retaining the original source binding,
complete cells/lists and pointer-identical job/header owners. Body-list errors
retain their context as bound::Error::BodyLists; initial and completed-owner
admission errors remain bound::Error::Admission.

Selected::value hides after explicit refusal. check_deadline and consuming
finish freshly admit without added positive charge or renewed credit. finish
releases combined passive View and original owners for low-level reuse; those
copies grant no source, locator or publication authority. Future response
composition must consume private Selected rather than accept passive copies.
Failure or safe forgetting cannot manufacture such an owner. Caller backing
stays provisional and is not wiped; serialized responses, locators and whole
job publication remain separate increments.

### 1.119 Original-source response metadata replay

M06dn adds mime_response::response. Projecting::new consumes private
Selected with a fresh real Tick; no passive View, replacement source, node,
header limit or budget argument can enter. It retains the original source,
base, header limit, complete cells, selected lists and original work/header
owners. Caller backing and scratch remain separately admitted.

next freshly admits before choosing exactly the next original ordinal. It
reuses Structure::metadata to clip only the original immutable source with
its traversal context and header limit. Part delegates bounded polling and
fresh consuming metadata finish, returning the original descriptor,
retained metadata and its already-complete classified node. It does not
classify or charge a node again. Metadata replay spends the original
allowances, including rereads and retained output; no credit is renewed.
Only successful consuming finish advances the ordinal. Child refusal or
safe forgetting retires the whole replay owner, including after Complete.

Projecting::finish freshly admits before requiring every original part.
Only complete replay creates private Projected, which retains the original
source and allowances. Explicit refusal hides its passive complete view;
its consuming finish also freshly admits before low-level owner release.
Earlier copied metadata and lists remain provisional, and caller bytes are
not wiped. This owner proves complete metadata visitation, not serialized
JSON, locator authority or publication. A composing response job must bind
its output framing and retention to that visitation separately. The next
framer wraps each live Part before its metadata window is reused; consuming
Projected alone cannot serialize metadata or prove response completion.

Six Copy/Clone guards and a rejected passive constructor pin exclusivity.
The replay plus its one metadata child and HeaderBudget fits the existing
8 KiB parser reservation; Projected has a compiled 256-byte ceiling. These
are layout ceilings, not native stack or RSS measurements. Whole metadata
retention, JSON structure framing, authenticated locators and final response
publication remain subsequent boundaries.

### 1.120 Original replay metadata JSON member framing

M06do adds mime_response::part_json::Cursor::new. It consumes only a live mime_response::response::Part
with a fresh Tick and requires healthy complete metadata. It retains that
child's original descriptor, classified node, immutable windows, work/header
owners, scratch and parent abandonment latch. No passive metadata view,
replacement field, source or allowance can construct it.

poll returns bounded Progress into caller output. The fixed member sequence
is partId, size, type, charset, name, disposition, cid, language, location,
without enclosing braces. Multipart partId is null; other IDs are quoted
original decimal preorder ordinals. size is the descriptor's exact decoded
size, checked against JMAP's 2^53-1 UnsignedInt ceiling. Absent charset
defaults to us-ascii when Content-Type is absent or text/* has no selected
label; absent non-text labels are null. A selected valid charset label keeps
its original spelling. Other optional absent metadata is null; present empty
strings stay present.
Type, charset, filename and disposition use the shared JSON string frame over
already selected metadata, preserving its normalization and control policy.
CID, language and location copy only this child's complete retained JSON.
Every wire byte of this new fragment, including copied retained JSON,
consumes original output allowance before copying. Retained metadata output
and subsequent fragment emission are distinct operations. Metadata scalar
reads, bounded text-type inspection and fixed control work spend the original
HeaderBudget, with no reset. Generated digits/defaults are not source I/O.
Child string-source refusals map to Admission; framing-state faults retain
Serialization context.

One turn polls at most one scalar (four bytes), inspects at most five text-
type bytes, or copies at most 64 bytes. At most 21 interpretation steps and
two records are charged per turn. Empty output
performs fresh zero-count admission without progress or positive charge.
Cached Complete is inert; explicit check_deadline and consuming finish
freshly admit, including after Complete. Any refusal hides value and latches
the parent's first error. Premature finish and safe abandonment also retire
the parent. Only successful fresh whole-fragment finish advances its ordinal
and returns passive End plus the original work/header/scratch owners.

The three constructor/exclusivity doctests and allocation probe pin this
boundary. Cursor plus HeaderBudget has a compiled 1 KiB layout ceiling.
Emitted bytes remain provisional through whole-job publication and are not
wiped. This fragment is not a complete EmailBodyPart: braces, blobId,
subParts, requested-property selection, raw headers, body-list/tree
composition, authenticated locators and final response publication remain
subsequent work. Projected continues to prove visitation alone.

### 1.121 Fixed original replay fragment retention

M06dp adds mime_response::part_window. Cursor::new consumes only a live
response Part with complete metadata and a fresh Tick, and binds one
separately reserved caller output window. It constructs the framer
privately; an already advanced framing cursor cannot be rewrapped into a
new retention window. No source, metadata, field or budget replacement
enters.

poll drives the same bounded framing turns into the shared td-json
Window, recording only reported written bytes. It adds no parsing,
serialization, output debit or new budget. Fresh admission precedes
capacity inspection; window exhaustion is sticky
bound::Error::ResponseCapacity. Partial bytes remain caller-owned and
provisional but no public value exposes them.

Only complete healthy framing exposes a passive Retained view.
Cached Complete is inert; explicit checks and consuming finish
freshly admit even after completion. For a retention-bound child, whole
retention finish alone consumes the framer, clears parent abandonment,
advances the original ordinal and returns the same work/header/scratch
owners. Premature consumption, any refusal or safe forgetting retires
the original parent; copied bytes are not wiped. Exact-fit windows
succeed, while every shorter window refuses. Bare Part and framer
finishes still advance visitation without retention; later composition
must require the live retention handoff to prove storage.

Cursor plus HeaderBudget has a compiled 1 KiB ceiling; caller output
storage is separately admitted. Three compile-fail guards pin
exclusivity and reject rewrapping an advanced framer. Rust allocation
qualification covers seven additional trials after the 27 earlier
ordered MIME/framing trials. Retained remains passive: enclosing
body-list/tree composition must wrap the live retention handoff and its
original replay owner. Braces, locators, subparts, property selection
and whole-response publication remain external.

### 1.122 Whole original fragment collection

M06dq adds mime_response::part_collection. Cell::new binds one separately admitted
caller output window. Collecting::new consumes only the original
Selected owner and exactly one cell per original ordinal, freshly
admitting before capacity inspection. Too few cells refuse
ResponseCapacity; surplus cells refuse InvalidState. Completed Projected
visitation and passive fragments cannot construct this owner. Cells are
private storage, never authority.

next binds original metadata replay and fresh fragment retention to that
ordinal's cell. One child serially uses the original metadata backing
and Scratch; its phases cannot escape to skip retention. Only fresh
complete Child::finish stores its fragment and clears whole-collection
abandonment. The same original admission owners fund control steps and
the stored-cell record. No fragment wire byte is charged again by
collection. Refusal, premature finish or safe forgetting retires the
whole collection, including a completed child. Reused cells and extra
next calls refuse stickily.

Fresh whole finish creates exclusive Serialized only after every
original ordinal has been retained. Its passive View contains original
selection and cells in original preorder, with each complete fragment's
descriptor/node. Explicit checks and consuming finish freshly admit,
hide values after refusal and return the original work/header owners.
Complete bytes remain provisional: braces, subparts, body-list/tree
composition, authenticated locators, request property selection and
publication are later boundaries.

Collecting plus Child and HeaderBudget has a compiled 8 KiB ceiling;
Serialized fits 256 bytes. Caller cells/windows are separately admitted,
with no additional heap arena or source-sized internal storage. Seven
additional Rust allocation trials preserve the previous 34 intervals;
native-stack and RSS qualification remain separate.

### 1.123 Original body metadata composition

M06dr adds mime_response::body_json. Cursor::new consumes only live
Serialized ownership, freshly admitted; passive View cannot substitute.
Mode::Structure emits the bodyStructure member with original preorder
fragments and iterative subParts. Mode::Lists emits textBody, htmlBody,
attachments and hasAttachment from original selected leaf ordinals.
These are provisional metadata members: authenticated blob locators,
request property selection and complete JMAP publication remain pending.

Each nonempty-output turn funds at most one interpretation step, one
record and 64 newly emitted wire bytes in the original work/header
owners. Generated retained fragments incur no source or I/O debit.
Empty output freshly admits without progress or debit; cached Complete
is inert. Sticky refusal hides values. Explicit checks and consuming
finish freshly admit; only complete emission creates exclusive Composed.
Its finish returns original owners and passive collection evidence.
Composed proves emission completion, not retained composition bytes.

Structure uses 64 fixed parent ordinals and original depth-one root
identity, without recursion. Leaves have null subParts; original traversal
rejects childless multipart input before collection can exist. Complete is reported with the final byte, including
an exactly filled caller output window. Cursor plus HeaderBudget fits
1 KiB; Composed fits 256 bytes. Eight units cover tree/list bytes, widths,
exact fit, every prefix and five quota cutoffs, fresh final consumption
and maximum depth. Five compile-fail guards pin exclusivity and reject
passive input. Eight additional measured Rust allocation trials preserve
all earlier 41. Native-stack and RSS qualification remain separate.

### 1.124 Whole original body-member retention

M06ds adds mime_response::body_window. Cursor::new consumes original Serialized,
chooses one composition mode and privately constructs a fresh composer
bound to a separately admitted fixed output window. Advanced composers,
completed emission owners and passive evidence cannot substitute. Only
reported bytes advance shared Window; retention adds no wire charge.

Fresh admission precedes capacity, including full midstream windows.
Sticky capacity/resource refusal hides every partial value. Cached
Complete is inert; fresh complete finish creates exclusive Retained,
which owns original Composed and the whole retained member bytes. Its
explicit check and consuming finish freshly admit, hide values after
refusal and release original work/header owners. ViewBytes is passive,
forgeable and provisional through authenticated whole-job publication.
Safe forgetting cannot reconstruct the consumed original proof.

Cursor plus HeaderBudget fits 1 KiB; Retained fits 256 bytes. The whole
window is separately admitted, with no heap arena or new allowance.
Five units cover bare-composer byte/cost parity, every shorter capacity,
every prefix, fresh final consumption, deadline priority, step/record/wire
refusal cutoffs and zero-source/I/O debit. Six compile-fail guards pin
exclusive owners and reject advanced/emitted constructor substitution.
Eight additional
measured Rust allocation intervals preserve the earlier 49. Native stack,
RSS, authenticated locators and complete response publication remain open.

### 1.125 Original provisional direct-leaf locator candidates

M06dt adds mime_response::locators. Cursor::new consumes only original whole
Retained and takes a typed, untrusted parent BlobId plus exactly one
caller candidate slot per original ordinal. Fresh admission precedes
capacity; too few slots refuse ResponseCapacity, surplus InvalidState.
Passive member bytes or candidates cannot reconstruct the constructor.

Each poll maps one original descriptor. Multipart candidates have no
locator or wire ID. A leaf uses body_start minus original source base,
entity_end minus body_start and original transfer encoding, with checked
ranges against source length. Canonical p1 bytes identify encoded extents;
decoded size never substitutes. Unknown transfer encodings retain the
original identity rule. This adds no body decoding or nested p2 context.

Original work/header owners fund at most 69 interpretation steps, six
records and 69 newly generated wire bytes per poll, with no source/I/O
debit. The stored candidate record and complete wire are funded before
slot replacement. Refusal leaves each uncommitted slot unchanged, including
stale passive contents in reused backing; that backing is not a completed
result after refusal. It hides whole values and is
sticky. Cached Complete is inert; fresh complete finish yields exclusive Mapped.
Explicit checks and consuming release freshly admit and return original
retained bytes, evidence, candidate backing and admission owners.

Candidate remains passive and provisional, with read-only ordinal/locator
getters and consistent stored wire bytes. View remains forgeable. The supplied
parent ID is not authenticated. Actual issuance/download/reuse must bind
whole source identity and live authorized parent access as WIRE.md
requires; in-range coordinates alone grant nothing. Mapped retains the
original private completion for that subsequent boundary.

Candidate fits 128 bytes; Cursor plus HeaderBudget and one temporary
Candidate fits 1 KiB; Mapped fits 256 bytes. Caller slots are separately
admitted, with no heap or additional allowance. Six units cover literal
IDs for all transfer tags, empty bodies/final CRLF, encoded lengths,
base normalization, multipart nulls, every prefix, constructor capacity
and expiry, exact positive quota refusal and no source/I/O debit. Five
compile-fail guards pin exclusive ownership and reject passive input.
Eight additional measured Rust allocation trials preserve the earlier
57. Native-stack/RSS and authenticated response publication remain open.

### 1.126 Original resident source identity with a retained file pin

M06du adds mime_response::pinned. Its constructor consumes original
Mapped and an actual complete PinnedBlob. Root/account authorization and
query admission remain caller obligations before open_blob_input; a
present row or matching digest does not establish email visibility or
upload-lease access. Neither released candidates nor a passive view can
supply the original completion. Refuse differing parent IDs or lengths
before digest creation, then hash the complete original resident source,
including headers and encoded bodies, against the pin's authoritative
digest. A parsed sub-entity cannot flatten into root-file coordinates.

Each poll visits at most 4096 bytes, charging original work I/O for every
visited byte, one interpretation step with carried record credit and one
explicit record before digest update. No new allowance, wire output,
header-source debit or filesystem replay is introduced. The owned
Provider SHA-256 and fixed digest comparison stay behind the existing
crypto port. The generic constructor checks the same combined Cursor, HeaderBudget
and final-digest 1 KiB envelope as the Provider size assertion.
Crypto errors discard the digest and retire this cursor.

PinnedBlob::check_deadline samples its own retained query clock before
and after a no-I/O fence. It preserves existing sticky deadline/source
failure and monotonic regression semantics. The binding checks both
original supplied-tick admission and this actual clock before work,
then checks the parent clock after digest work, even when that work
returns an error; post-work parent failure takes precedence. Only a
matching full digest completes. Cached Complete is inert and passive
views remain provisional. Explicit checks and consuming finish are
fresh. Caller workers must supply a fresh Tick for original-job admission
and couple the pin query deadline to their enclosing request; the pin's
independent clock fence does not authenticate that supplied Tick.
A private Bound retains both original Mapped and actual pin;
its checked low-level release returns these exclusive owners, not an
issuance/publication permission.

The Provider cursor plus original HeaderBudget and final digest fits
1 KiB; Bound fits 768 bytes. Original candidate/metadata windows and
resident source stay live throughout, and the pin retains its already
admitted pooled view. There is no new explicit heap allocation in the
wrapper; this increment does not qualify binding-path Rust/native
allocations, stack or RSS. Earlier 65 locator/metadata allocation trials
remain separate evidence. Complete service admission, current access
policy, nested p2 contexts and authenticated response publication follow.

### 1.127 Original source-binding Rust allocation qualification

M06dv measures eight source-binding intervals in the dedicated sequential
Rust allocation process. The probe compiles the production MIME owners
and filesystem pin in one test-only source graph, preserving their
private constructors and real pooled-view custody. File creation,
complete pin verification, source construction, original mapping and
all backing precede the measured interval. Counting starts before
binding construction and ends after consuming release or refusal drops
its retained state and PinnedBlob descriptor. The enclosing PooledRead,
CommittedView and scratch lease release stay outside this interval.

Cover healthy zero and near-u64 source bases, partial expiry, completed
cursor expiry, Bound expiry, a same-length changed resident byte, exhausted
original I/O work and constructor pin expiry. Each interval compares all
Rust allocation counters unchanged. These eight intervals supplement the
existing 65 metadata/locator trials. They do not qualify filesystem setup,
native allocation, worker stacks, RSS, whole-job admission or authorized
locator publication. The production API and allocation shim are unchanged.

Only the eight listed intervals are qualified. ID/length mismatch,
supplied-Tick expiry, interpretation-step exhaustion, digest creation/
update/finish faults, post-crypto clock expiry, clock Invalid/Busy,
premature finish, sticky/cached re-poll and oversized generic crypto
state remain unmeasured. The near-u64 case repeats healthy binding after
cold coordinate normalization; it covers no additional hashing branch.

### 1.128 Source-bound original part-member emission

M06dw adds mime_response::metadata. Cursor::new consumes only
original Bound, freshly checking both original supplied-Tick admission
and actual pin clock before accepting a one-based original ordinal.
Passive views, candidate arrays and completed emission alone cannot
substitute. Emit that original retained metadata fragment followed by
blobId: the canonical quoted p1 locator for a direct leaf, or JSON null
for a multipart container. Braces, subParts, whole tree/list composition,
request property selection and authenticated publication remain later.

Every nonempty-output turn funds one interpretation step with carried
control credit, one explicit record and at most 64 new wire bytes before
copying. Retained generated metadata and locator bytes have no source/I/O
debit. Empty output freshly admits without charge or progress; cached
Complete is inert. Check the actual parent clock again after work,
including a work error; post-work parent failure takes precedence.
Outputs written before refusal are provisional and must be discarded.
Sticky refusal hides values and cannot refund or renew original work.
Completion is reported with the final byte, including exact output fit.

Fresh consuming finish creates private Member retaining original Bound,
actual descriptor and chosen ordinal. Member explicit checks and checked
release remain fresh; release returns the original Bound and ordinal.
This proves completed emission, not retained output or access permission.
Original source, fragments, candidate cells, prior whole-member window
and pooled-view borrow overlap throughout. Cursor, HeaderBudget, five
borrowed segments and a 64-byte output slice fit 1 KiB; Member fits
768 bytes. These size caps do not establish ledger reservations.

Eight dedicated sequential Rust allocation intervals measure construction,
emission and binding-owned descriptor teardown after cold source matching
and fixture setup. Cover leaf/container success, partial/completed/Member
expiry, wire refusal, invalid ordinal and constructor pin expiry. Enclosing
pooled-view/scratch-lease cleanup stays outside counting. These intervals
supplement the earlier 65 metadata/locator and eight binding intervals;
no native/stack/RSS, full request, authorization or publication claim follows.
Only the listed eight intervals are measured. Supplied-Tick and step/record
refusal, post-copy clock expiry, Invalid/Busy clocks, sticky/cached polling
and premature finish remain unqualified for Rust allocation.

### 1.129 Whole source-bound part-member retention

M06dx adds mime_response::metadata_window. Cursor::new consumes original Bound,
selects one ordinal and privately constructs a fresh member emitter
bound to a separately admitted caller window. Advanced emitters and
completed emission owners cannot replace original Bound. Window advances
only by reported bytes, adding no wire charge. Fresh original-job and
actual-pin checks precede capacity refusal; source emitter post-work
pin fences retain deadline precedence over quota failure. Refusal hides
all partial values. Cached Complete stays inert; explicit checks and
consuming finish remain fresh, including an exactly full window. Any
copied bytes after refusal remain provisional and must be discarded.

Complete finish yields private Retained, holding original Member and
whole member bytes. Its fresh low-level release returns original Bound,
ordinal and retained bytes. View is passive and provisional; whole
collection/composition, request selection, current access authorization
and publication still follow. The caller reservation uses checked
fragment length plus 81 bytes for the largest suffix: ten blobId prefix
bytes, 69 canonical p1 bytes and two quotes. This conservative bound also
covers multipart null. It grants no source or completion authority and
must overlap existing original source/windows/candidates and pin leases.

Cursor, HeaderBudget, five borrowed segments and 64 output bytes fit
1 KiB; Retained fits 768 bytes. This is a conservative size envelope;
the wrapper writes directly into caller backing and allocates no extra
64-byte scratch. These caps do not enforce whole-job ledger reservations. Eight sequential Rust allocation intervals measure
construction through retention/release/refusal and descriptor teardown
after cold original mapping, source matching, backing and filesystem
setup. Cover near-u64 leaf and zero-base container success,
partial/completed/Retained actual-pin-clock expiry, short window, wire
refusal and constructor pin expiry. Enclosing pooled
view and scratch lease release remain outside counting. Only these
intervals are qualified. Supplied-job-Tick expiry, record/step refusal,
post-work pin expiry, invalid ordinal, premature finish, cached re-poll,
Retained::check_deadline and clock Invalid/Busy remain unmeasured, along
with native allocation, stack, RSS and complete-service admission.
Earlier 65 metadata/locator, eight binding and eight member intervals
remain separate evidence.

### 1.130 Whole original source-bound member collection

M06dy adds mime_response::metadata_collection. Collecting consumes only
original Bound and an exactly sized separately admitted mutable Cell
array, one fixed output window per original candidate. Fresh supplied
job Tick and actual pin admission precede cardinality refusal. It does
not accept passive bytes or one completed member as whole collection.
Each next borrows the parent exclusively and transfers its original
Bound into a fresh private retention cursor for the next ordinal.
The parent records Abandoned until that child freshly completes; drop,
including after cached completion, cannot authorize a skipped ordinal.

Child owns the original descriptor and all prior backing while emitting
and retaining at most 64 bytes per funded turn. Child finish freshly
consumes complete Retained and funds one interpretation step with fresh
zero control credit, spending one prepaid control record and one
explicit acceptance record in original work: exactly one step and two
records, without wire/source/I/O debit. Credit is not carried from
emission or between acceptances. It then samples the actual
pin even if funding refused. The post-acceptance pin refusal wins.
Only success installs the whole original member into its original Cell,
returns the original Bound, advances exactly one ordinal and clears
Abandoned. Cell values remain passive and provisional; used cells
cannot renew custody. Failure hides child values and poisons the parent.

Whole finish freshly requires every ordinal, yielding exclusive
Serialized with original Bound, all cells and actual descriptor. Its
explicit admission and consuming low-level release stay fresh. The
released cells alone supply no whole collection/source/composition or
current access authority. Tree/list composition, request property
selection and authenticated publication still follow.

Collecting plus Child plus HeaderBudget fit 2 KiB; Cell fits 64 bytes and
Serialized fits 768 bytes. These caps do not reserve whole-job memory
or qualify worker stack. Eight Rust allocation intervals cover near-u64
leaf, zero-base multipart, partial/completed child expiry, completed
parent expiry, Serialized expiry, abandonment and constructor pin
expiry. Original mapping/matching, backing and file setup stay cold;
descriptor teardown is counted, enclosing pooled-view/scratch lease and
fixture cleanup are outside. Only those intervals are qualified; cell
cardinality/reuse, capacity/quota failures, supplied-Tick expiry,
post-acceptance expiry, clock Invalid/Busy, premature finish and cached
polls have unit/type coverage or remain future cases, not independent
allocation qualification. Native allocation, stack/RSS, complete job
admission and publication remain open. Earlier 65 metadata/locator and
eight each binding, emission and whole-retention intervals are separate.

### 1.131 Original source-bound whole tree/list emission

M06dz adds mime_response::metadata_json. Cursor consumes
only original Serialized, selecting Structure or Lists, and retains
that whole collection and actual descriptor throughout emission.
The existing metadata composer and this source-bound composer share
one crate-private bounded Frame and source adapters. Framing, checked
ordinal/depth/parent walk, ordered list membership, commas, braces,
subParts and hasAttachment remain one implementation. The old metadata
API and its byte/funding behavior remain unchanged; its prior inline
framing mechanism is deleted in the same increment.

The new adapter copies only whole source-bound Cell members, requiring
original part/fragment/candidate/member ordinal agreement. Each poll
funds one original interpretation step with carried control-record
credit and at most 64 generated output bytes before copying, with no
source/I/O debit. Fresh original-job and actual-pin checks precede work;
the actual pin is sampled directly afterward even if original work
refused, and its error wins. Empty output freshly admits without charge
or progress; cached Complete is inert. Failure hides values; every copied
prefix remains provisional and must be discarded on refusal.

Fresh complete finish creates exclusive Composed, which retains original
Serialized and Mode through explicit admission and fresh low-level
release. Emission completion proves neither retained whole wire bytes
nor current access/publication authority. Whole response retention,
request property selection and authenticated publication still follow.

Cursor plus HeaderBudget and 64 output bytes fit 1 KiB; Composed fits
768 bytes. These caps do not reserve a whole ledger or qualify stack.
Eight Rust allocation intervals cover near-u64 digest-tree Structure,
zero-base digest Lists, actual-pin-clock expiry after a copied prefix,
completed Cursor expiry, Composed expiry, explicitly checked released
Serialized expiry, wire refusal and constructor pin expiry. Setup,
original mapping/matching/whole member collection and backing stay cold;
descriptor teardown is counted, enclosing pooled view/scratch lease,
cell-array and fixture cleanup remain outside. Only those intervals are
qualified. Supplied-Tick expiry, interpretation/record refusal, post-turn
pin expiry including simultaneous quota failure, premature finish,
empty/cached polls, clock Invalid/Busy and other tree/list variants are
not independently allocation-qualified. Earlier 65 metadata/locator and
eight each binding, member emission, whole retention and collection
intervals remain separate evidence. Native/stack/RSS and complete job
admission/current publication remain open.

### 1.132 Whole source-bound tree/list members in a fixed window

M06ea adds mime_response::metadata_json_window.
Only original complete Serialized can start a fresh private composer
and shared td-json Window. Advanced emission or Composed cannot establish
retention of a lost prefix. Keep original collection, source/fragments,
all fixed member cells and actual descriptor alongside the whole output
window. The separately admitted window is additional overlapping backing,
not a new source arena or a publication capability.

Advance only successfully reported original generated bytes. Retention
adds no interpretation, control-record, wire or source/I/O debit beyond
the private composer. Every unfinished poll freshly admits original job
and actual pin before capacity; capacity refusal is sticky and hides
whole values. Copied prefixes remain provisional and must be discarded.
An exact window completes; cached Complete is inert. Fresh complete
finish creates exclusive Retained. Explicit checks and consuming final
release freshly admit both deadline domains, returning original
Serialized, Mode and the complete borrowed window bytes. The released
collection retains the actual descriptor; passive ViewBytes has no
current access or publication authority. These bytes are whole property
members, not a complete JMAP response object.

Cursor plus HeaderBudget fits 1 KiB; Retained fits 768 bytes. These
compiled caps are not full-ledger or stack proofs. Eight sequential Rust
allocation intervals cover near-u64 digest Structure, zero-base digest
Lists, actual-pin expiry after a copied prefix, completed Cursor expiry,
Retained expiry, explicitly checked released Serialized expiry, empty
window capacity refusal and constructor pin expiry. Original mapping,
matching, whole collection, member/backing and output window preparation,
full verification and filesystem setup stay cold. Descriptor teardown is
counted; pooled view, scratch lease, cell array and fixture cleanup stay
outside. Only these intervals are measured. Short positive/exact-fit
windows, supplied-Tick expiry, interpretation/record/wire refusal,
post-turn pin expiry including simultaneous quota failure, premature
finish, cached polls, clock Invalid/Busy and other source/mode variants
are not independently allocation-qualified. Earlier 105 metadata and
source-bound intervals remain separate evidence. Native/stack/RSS,
complete admission, request selection and authenticated publication
remain open.

### 1.133 Requested source-bound body-list properties

M06eb adds mime_response::selected_body_json.
Properties is a pure four-boolean caller selection for textBody,
htmlBody, attachments and hasAttachment. Explicit NONE selects nothing;
ALL selects all four. There is no implicit default or request-parser
default policy here. It neither authorizes an email nor owns original
source. Only complete original Serialized starts the
exclusive selected Cursor. Retain the original collection and actual
file pin while using the same bounded Frame with its private selection.
Emit selected keys once, in that canonical order, with no leading or
trailing comma and no unselected key. Whole leaf members, original list
membership and hasAttachment semantics remain unchanged.

The existing metadata and source-bound full-list constructors select all
four keys with unchanged bytes, turns, control credit and funding. Each
unfinished nonempty turn still funds one original interpretation step
and at most 64 actual generated wire bytes before copying, with no new
source/I/O debit. Adapter pre/post pin fences and sticky refusal remain
those of the original source-bound composer. Last selected list closure
completes on its final copy, without an extra output-demanding turn.
An empty selection freshly admits at construction and is already
complete with zero generated bytes or work charge. Cached Complete is
inert; explicit checks and consuming finishes remain fresh in both
original-job and actual-pin deadline domains, including the empty
selection.

Fresh complete finish yields exclusive selected Composed, keeping
Properties and original collection through final release of original
Serialized and Properties. Its passive view includes original cells;
selection describes generated keys, not a new source binding. Emission
still proves neither whole retained output nor current access. This is
only selection of these four list properties; bodyStructure routing,
per-part bodyProperties, JSON request parsing/unsupported-property errors,
whole selected-output retention and authenticated publication follow.

Cursor plus HeaderBudget and 64 output bytes fits 1 KiB; Composed fits
768 bytes. These compiled caps are not ledger or stack proofs. Eight
sequential Rust allocation intervals measure near-u64 digest textBody
only, zero-base empty selection, actual-pin expiry after a copied prefix,
completed Cursor expiry, Composed expiry, explicitly checked released
Serialized expiry, wire refusal and constructor pin expiry. Original
mapping/matching/collection, backing, full verification and filesystem
setup stay cold. Descriptor teardown is counted; pooled view/scratch
lease, cell array and fixture cleanup remain outside. Only those
intervals are measured. Other subsets, widths/sources, supplied-Tick
expiry, interpretation/record refusal, post-turn pin expiry, empty/cached
polls and explicit completed-owner checks are not independently
allocation-qualified. Earlier 113 intervals remain separate evidence.
Native/stack/RSS, full admission and current publication remain open.

### 1.134 Whole requested source-bound list retention

M06ec adds
`mime_response::selected_body_window`
beside the source-bound list selector.
Only original complete Serialized and pure Properties start a fresh
selected frame through the shared source-bound retention core and
td-json Window. Advanced or emitted-only
owners cannot retain a missing prefix. Exclusive Cursor retains original
collection, actual pin and caller-owned output; ViewBytes holds the
selection, complete provisional members and passive original view.
Successful reported bytes alone advance the window. Exact fit completes
on the last selected copy. Empty selection freshly admits and completes
into even a zero-byte window without charge. Cached completion is inert;
explicit checks and consuming finishes freshly validate both original
job and actual pin, including empty selections.

Fresh admission precedes capacity on unfinished turns. Capacity, funding
and pin errors stay sticky and hide the whole value. Retention adds no
interpretation, record, wire or source/I/O debit to selected emission.
Fresh finish yields exclusive Retained; fresh release returns original
Serialized, unchanged Properties and complete borrowed bytes. Released
bytes remain passive. These are selected property members, not complete
JMAP objects or authorization. bodyStructure routing, per-part
bodyProperties, JSON request parsing, full admission and current
publication remain later.

Cursor plus HeaderBudget fits 1 KiB; Retained fits 768 bytes. Compiled
caps are not stack or admission-ledger proofs. Eight sequential Rust
allocation intervals measure near-u64 digest textBody, zero-base empty
selection into zero capacity, actual pin expiry after a copied prefix,
completed Cursor expiry, Retained expiry, explicitly checked released
Serialized expiry, nonempty zero-window capacity and constructor pin
expiry. Original mapping/matching/collection, backing, verification,
filesystem and output preparation stay cold. Descriptor teardown is
counted; pooled view/scratch lease, cells and fixture cleanup remain
outside. Only these eight intervals are measured; other subsets,
capacities/sources, supplied-Tick expiry, funding refusal, premature
finish, cached Complete polls, post-turn pin expiry, clock Invalid/Busy
and explicit completed-owner checks remain allocation-unqualified.
Earlier 121
intervals are separate evidence. Native allocation, stack/RSS, full
admission and authorized publication remain unqualified.

### 1.135 Requested source-bound tree and list routing

M06ed adds mime_response::requested_body_json for a pure five-boolean bodyStructure,
textBody, htmlBody, attachments and hasAttachment selection. NONE emits
no members; ALL emits all five, in that order. This is a typed caller
selection, not request JSON decoding/default policy or per-part
bodyProperties selection. Only original complete Serialized starts its
exclusive Cursor. The same shared Frame emits the tree first when
selected, then requested list keys with the correct separator. A tree
without lists preserves legacy Structure bytes, turns and funding; list
ALL without a tree preserves legacy full-list behavior. Tree/list
transition is bounded and separately charged, with no source/I/O work.
There is no reconstructed source owner or second byte engine.

Every unfinished nonempty turn freshly admits original-job and actual
pin, charges one interpretation step and at most 64 generated bytes before
copy, checks the actual pin afterwards, and keeps refusals sticky.
Empty selection freshly constructs complete with no generated bytes or
work. Cached Complete is inert; explicit and consuming owner checks
remain fresh. Final selected copy completes without demanding another
output turn. Fresh finish yields exclusive requested Composed, carrying
selection and original collection through fresh Serialized/Properties
release. Emitted members and passive views carry no publication right or
complete JMAP object. Whole requested retention, request JSON parsing,
per-part properties, full admission and current publication remain later.

Cursor plus HeaderBudget and 64 output bytes fits 1 KiB; Composed fits
768 bytes. These compiled caps are not admission-ledger or stack proofs.
Eight sequential Rust allocation intervals measure near-u64 digest tree
plus textBody, zero-base NONE, actual pin expiry after a copied prefix,
completed Cursor expiry, Composed expiry, explicit checked released
Serialized expiry, wire refusal and constructor pin expiry. Original
mapping/matching/collection, backing, full verification and filesystem
setup stay cold. Descriptor teardown is counted; pooled view/scratch,
cells and fixture cleanup remain outside. Other subsets, widths/sources,
empty NeedOutput polls, unfinished explicit deadline checks, supplied-Tick
expiry, step/record refusal, post-copy expiry and explicit complete-owner
checks remain allocation-unqualified. Earlier 129
intervals are separate evidence; native allocation, stack/RSS, full
admission and authorized publication remain unqualified.

### 1.136 Whole requested source-bound tree/list retention

M06ee adds mime_response::requested_body_window. Only original complete Serialized and
pure requested Properties create a fresh tree/list composer into the
shared Window. Advanced or emitted-only owners cannot retain a missing
prefix. Exclusive Cursor keeps original cells/source backing, actual pin
and whole caller-owned output. Its passive ViewBytes includes Properties,
complete provisional members and original view. Advance only successful
reported output with no extra interpretation, record, wire or source/I/O
charge. Fresh admission precedes unfinished capacity, all failures stay
sticky and hide the value, and final-copy exact fit completes. NONE
freshly constructs complete into even zero capacity; cached completion
is inert. Explicit checks and all consuming finishes freshly validate
both original-job and actual-pin deadlines, including NONE.

Fresh complete finish yields exclusive Retained, and fresh release
returns original Serialized, unchanged requested Properties and complete
borrowed bytes. There is no reconstructed source owner, second byte or
window engine, complete JMAP object or publication authority. Per-part
bodyProperties, request JSON decoding, full admission and current
authenticated publication remain later.

Compiled Cursor plus HeaderBudget fits 1 KiB and Retained fits 768 bytes,
without claiming a whole ledger or stack proof. Eight sequential Rust
allocation intervals measure near-u64 digest tree+textBody, zero-base
NONE into zero capacity, actual pin expiry after copied output, completed
Cursor expiry, Retained expiry, explicitly checked released Serialized
expiry, nonempty zero capacity and constructor pin expiry. Original
mapping/matching/collection, backing, full verification, filesystem and
output preparation stay cold. Descriptor teardown is counted; enclosing
pooled view/scratch, cells and fixture cleanup stay outside. Only these
eight intervals are qualified; other subsets, capacities/sources,
supplied-Tick expiry, funding refusal and explicit completed-owner
checks remain allocation-unqualified. Earlier 137 intervals are separate.
Native allocation, stack/RSS, full admission and authorized publication
remain unqualified.

### 1.137 Selected original source-bound part fields

M06ef adds
`mime_response::selected_metadata`.
Only original source-matched Bound and its original ordinal enter the
exclusive Cursor. Pure explicit Properties select partId, size, type,
charset, name, disposition, cid, language, location and blobId in the
existing canonical member order. NONE and ALL are caller selections, not
missing/null JSON request defaults. headers, header:* and subParts are
outside this ten-field surface. This emits provisional part members, not
complete BodyPart/JMAP objects or current publication authority.

Fresh original-job and actual-pin admission precedes ordinal validation.
Construction validates original fragment/descriptor/candidate identity
without work debit. ALL retains the original byte/turn/five-cost path;
NONE freshly completes without work or bytes and retains source custody.
Other subsets index only td-generated fixed metadata, not source/request
JSON. The fixed nine-span index checks canonical keys, tracks quoted
escapes and one language-array level, and accepts only u32-representable
fragment extents. It scans at most 64 generated bytes per nonempty turn,
funded by that many interpretation steps, one record and existing
control credit. Selected copying costs one step, one record plus control
credit and exactly the reported wire bytes, with no source/I/O charge.
Each copy writes at most 64 bytes, visiting at most ten fields per byte;
blobId length/copy each traverse at most four original suffix segments.
Selected commas occur only between present fields; quoted text, arrays,
locator spelling and container nulls stay byte-identical.

Empty unfinished output freshly checks both domains without indexing or
fees. Every funded scan/copy turn samples the actual pin afterwards,
including work refusal; errors stay sticky and hide value. The final
selected copy completes; cached Complete polls are inert. Explicit
checks and both consuming owner boundaries remain fresh, including NONE.
Completed exclusive Member retains source and unchanged Properties;
fresh release returns original Bound, ordinal and that selection. No
passive, advanced or reconstructed source can enter the constructor.

Cursor plus HeaderBudget, 64 output bytes and five original segment
references fits 1 KiB; Member fits 768 bytes. The shared base Cursor
also carries the optional fixed index on legacy ALL paths. These caps
prove neither whole admission nor stack/RSS. Eight sequential Rust
allocation intervals measure near-u64 leaf partId+blobId, zero-base
NONE, actual pin expiry after selected copied output, completed Cursor
expiry, Member expiry, explicitly checked released Bound expiry, wire
refusal after indexing and constructor pin expiry. Original generated
metadata, matching, backing and file setup stay cold. Descriptor
teardown is counted; pooled view/scratch, cells and fixture cleanup
remain outside. Other subsets/sources/widths, supplied-Tick expiry,
interpretation/record refusal, post-turn/clock failures, malformed
generated metadata, invalid ordinals, empty/cached polls, premature
finish and other explicit checks remain allocation-unqualified. Earlier
145 intervals are separate evidence. Native allocation, full admission,
per-part selection integration into whole retention/collection/tree
composition, request parsing and current authenticated publication
remain unqualified.

### 1.138 Whole selected original source-bound part fields

M06eg adds
`mime_response::selected_metadata_window`.
Accept only original source-matched Bound, original ordinal, explicit
ten-field Properties and separately caller-owned/admitted output. A
private original-bound constructor configures a fresh selected emitter;
no advanced Cursor or emitted-only Member can retain a missing prefix.
Both legacy ALL and selected whole retention use the same Window, poll,
sticky failure and finalization core.

Fresh original-job and actual-pin admission precedes output capacity and
ordinal validation. ALL preserves the original path; NONE freshly
completes into zero capacity without work or bytes, retaining source
custody. Other subsets fund generated indexing and selected copying
exactly as §1.137. Whole retention advances only successful reported
output without adding any of the five costs. Exact fit completes on the
final selected copy. Unfinished zero/short capacity refuses exactly and
stickily; healthy premature finish refuses. Fresh deadline checks
precede unfinished capacity checks. Cached Complete polls remain inert,
while explicit checks and consuming Cursor/Retained/released Bound
boundaries remain fresh in both domains.

Complete passive View reports unchanged Properties, ordinal and whole
members. Fresh finish returns exclusive Retained with the same labels;
fresh release returns original Bound, ordinal, Properties and complete
caller-window bytes. Original collection/backing/candidates and actual
descriptor remain held throughout; emitted bytes are provisional part
members, not complete BodyPart/JMAP objects or publication authority.
headers, header:* and subParts, request defaults and parsing, whole
collection/tree integration, full admission and current authenticated
publication remain later.

Cursor plus HeaderBudget, five segment references and 64 output bytes
fits 1 KiB; Retained fits 768 bytes. These caps do not qualify worker
stack/RSS or whole admission. Eight sequential Rust allocation intervals
measure near-u64 leaf partId+blobId, zero-capacity NONE, actual pin
expiry after selected copied output, completed Cursor expiry, Retained
expiry, explicitly checked released Bound expiry, nonempty short
capacity and constructor pin expiry. Original matching/generated
metadata/backing, filesystem and output preparation stay cold.
Descriptor teardown is counted; pooled view/scratch, cells and fixture
cleanup stay outside. Other sources/subsets/capacities, supplied-Tick
expiry, interpretation/record/wire refusal, post-turn/clock failures,
invalid ordinals, cached polls, premature finish and other explicit
checks remain allocation-unqualified. Earlier 153 intervals are
separate. Native allocation, stack/RSS, full admission and current
authenticated publication remain unqualified.

### 1.139 Whole original selected-field collection

M06eh adds
`mime_response::selected_metadata_collection`.
Original Bound, one immutable explicit ten-field Properties and
caller-owned/admitted exact-count Cell windows construct exclusive
Collecting. Cell is the shared legacy window slot; its value is passive
ordinal/member bytes. Complete selected collection View supplies the
common Properties beside the original source-bound collection view. No
passive view or emitted-only retained member reconstructs an original
collection.

The shared core privately accepts pure Properties and starts each child
from the original Bound and next original ordinal, using §1.138's whole
selected emitter. Legacy public construction passes ALL. No second
collection, abandonment or acceptance mechanism is introduced. Fresh
original-job and actual-pin checks precede short/extra cell cardinality.
Only successful fresh complete Child finish commits its whole bytes and
advances the parent. Original selection/candidates/backing and actual
descriptor return to the parent at each accepted ordinal. Dropped child,
premature finish, window/work refusal and deadline failures poison the
parent with the exact shared sticky error. Every original ordinal must
complete; neither skipped slots nor prefilled slots can renew the proof.

NONE children freshly complete into zero-capacity windows without
emission work. Exactly one Cell per original ordinal remains required.
Child acceptance still pays the unchanged one interpretation step, one
record and its control fee: the existing zero-credit handoff totals two
records, with zero source/I/O or wire fees. ALL preserves legacy turns
and costs. Other masks preserve bare selected per-turn five-cost debits
plus those original handoff fees. Complete child polls remain inert;
explicit and consuming child/parent/Serialized/released Bound boundaries
stay fresh in both domains. Fresh complete Serialized release returns
original Bound, unchanged Properties and complete original slots.
Selection labels and bytes remain provisional and confer no current
publication authority.

Cell fits 64 bytes; Collecting plus Child and HeaderBudget fits 2 KiB;
Serialized fits 768 bytes. Original backing, windows and cells remain
separately caller-owned/admitted. These compiled caps do not prove
stack/RSS or whole admission. Eight sequential Rust allocation intervals
measure near-u64 multipart partId+blobId across both original ordinals,
zero-window NONE, actual pin expiry after selected copied child output,
completed parent finish expiry, completed Serialized expiry, explicitly
checked released Bound expiry, nonempty zero child capacity and
constructor pin expiry. Original source matching/generated
fragments/backing, filesystem, windows and cells stay cold. Descriptor
teardown is counted; pooled view/scratch, cells and enclosing fixture
cleanup remain outside. Other sources/subsets/capacities, supplied-Tick
expiry, cardinality, abandonment, handoff quotas, post-turn/clock
failures, cached polls, premature finish and other explicit checks
remain allocation-unqualified. Earlier 161 intervals remain separate.
Native allocation, stack/RSS, full admission, selected-field tree/list
composition, request defaults/parsing, headers/header:*/subParts and
current authenticated publication remain later or unqualified.

### 1.140 Original selected-field tree/list composition

These legacy constructors retain their structural scaffold. API §1.147
adds explicit subParts selection through the same engine; legacy
selection types still do not encode JSON request defaults.

M06ei adds
`mime_response::selected_metadata_json`.
Only complete §1.139 selected Serialized and an explicit Mode construct
exclusive Cursor. Preserve immutable per-part Properties, original
collection/cells/backing and actual descriptor through Cursor, Composed
and fresh complete release back to the selected Serialized. Passive
views and advanced legacy cursors cannot reconstruct the selected owner.

The facade privately delegates to the original source-bound composition
owner and shared frame. Every original member is already whole and
selection-complete. Structure walks the original tree; Lists visits the
original text/html/attachment ordinals and attachment flag. Both retain
the existing provisional structural subParts scaffolding. Ten-field
Properties select only metadata inside each object, not this scaffold.
NONE therefore emits objects with only structural subParts; it does not
remove original tree nodes or list entries. Omit the scaffold's leading
comma exactly when the whole selected member is empty. This is
provisional generated composition, not request bodyProperties
parsing/defaults or a complete JMAP response.

ALL preserves legacy bytes, turns and five-cost debits. Each funded turn
pays the existing interpretation step and control/record fee, with exact
copied wire bytes and no source/I/O fees; copy stays at most 64 bytes.
Empty unfinished output freshly checks both domains and spends no work.
Work refusal prevents completion; clock refusal hides completed views.
Cached complete polls stay inert. Explicit checks and consuming
Cursor/Composed/Serialized/released Bound boundaries remain fresh in
both domains. Only fresh completed Cursor finish constructs Composed.
Bytes and labels confer no publication authority.

Cursor plus HeaderBudget and 64 output bytes fits 1 KiB; Composed fits
768 bytes. Original backing, cell and member windows remain separately
caller-owned/admitted. Eight sequential Rust allocation intervals
measure near-u64 multi-node structure, NONE lists, actual pin expiry
after copied frame output, complete Cursor expiry, complete Composed
expiry, explicitly checked released Bound expiry, healthy empty output
and constructor pin expiry. Original matching/generated members,
filesystem, windows and cells stay cold. Descriptor teardown is counted;
pooled view/scratch, cells and enclosing cleanup remain outside. Other
masks/sources/capacities, supplied Tick expiry, quotas, premature
finish, cached polls, post-turn/clock faults and other explicit checks
remain allocation-unqualified. Earlier 169 intervals remain separate.
Native allocation, stack/RSS, full admission, request property
parsing/defaults, headers/header:*/subParts selection and current
authenticated publication remain later or unqualified.

### 1.141 Requested outer properties with selected part metadata

These legacy constructors retain their structural scaffold. API §1.147
adds explicit subParts selection through the same engine; legacy
selection types still do not encode JSON request defaults.

M06ej adds
`mime_response::requested_selected_json`.
Only complete selected Serialized and explicit five-property outer
Properties construct exclusive Cursor. The outer selection chooses
bodyStructure, textBody, htmlBody, attachments and hasAttachment; the
source retains its independent explicit ten-field metadata selection.
Passive complete views expose both selections beside original
collection/cells/backing. Fresh consuming release returns selected
Serialized and unchanged outer Properties. No passive view or advanced
legacy cursor reconstructs the selected owner.

Privately reuse the existing requested source-bound frame and its shared
funding, freshness, sticky refusal and complete acceptance. Canonical
outer order is structure, text, HTML, attachments and attachment flag.
Selected metadata remains byte-identical, including container nulls and
leaf locator spelling. The provisional structural subParts scaffold
remains independent of metadata selection under §1.140. Both selections
are pure explicit caller inputs; absent/null request defaults and JSON
parsing remain outside this API.

Outer NONE freshly constructs complete zero-work/zero-byte emission
while retaining original selected collection custody. Cached completion
polls remain inert; explicit checks and consuming boundaries stay fresh
in both original-job and actual-pin domains. Combined requests complete
the original tree before selected lists; neither selection label
changes. Metadata ALL preserves legacy requested bytes, turns and five
costs. Other masks preserve the existing one-step/control and exact wire
fees, zero source/I/O fees and at-most-64-byte copy. Empty unfinished
output checks freshness without work. Premature finish and work refusal
prevent completion; clock refusal hides completed views. Refusals retain
exact sticky errors. Bytes and labels confer no current publication
authority.

Cursor plus HeaderBudget and 64 output bytes fits 1 KiB; Composed fits
768 bytes. Original backing/windows/cells remain separately
caller-owned/admitted. Eight sequential Rust allocation intervals
measure near-u64 combined multi-node structure/lists, both selections
NONE, actual pin expiry after copied output, complete Cursor expiry,
complete Composed expiry, explicitly checked released Bound expiry,
healthy empty output and constructor pin expiry. Original
matching/generated member windows, filesystem and cells stay cold.
Descriptor teardown is counted; pooled view/scratch, cells and enclosing
cleanup remain outside. Other selections/sources/widths, supplied Tick
expiry, quota refusal, premature finish, repeated cached polls, post-turn/clock
faults and other explicit checks remain allocation-unqualified. Earlier
177 intervals remain separate. Native allocation, stack/RSS, full
admission, whole composed retention, request parsing/defaults,
headers/header:*/subParts selection and current authenticated
publication remain later or unqualified.

### 1.142 Whole requested output with selected part metadata

These legacy constructors retain their structural scaffold. API §1.147
adds explicit subParts selection through the same engine; legacy
selection types still do not encode JSON request defaults.

M06ek adds
`mime_response::requested_selected_window`
over §1.141's original selected collection. Cursor starts only from
complete selected Serialized, pure outer Properties and one caller-owned
fixed output window; an advanced requested Cursor, emitted-only Composed
or passive view cannot recreate a lost prefix. Privately reuse the
original whole requested retention core while preserving immutable
per-part metadata selection. Retained ViewBytes exposes outer
properties, complete provisional members and the original selected view.
Fresh release returns selected Serialized, unchanged outer properties
and the same complete borrowed output bytes; the collection still owns
original cells/backing and actual descriptor custody.

Reuse at-most-64-byte funded copies, exact five-cost debits and sticky
window/work/freshness errors. Exact windows succeed, spare tails remain
untouched and insufficient windows expose no whole value. Outer NONE
needs no output capacity or composition work. Completed polls remain
inert; explicit and consuming Cursor/Retained/Serialized boundaries
check both original-job and actual-pin domains, including NONE. Clock
refusal hides a completed view. Work and capacity refusal prevent
completion. Bytes and selection labels remain provisional through
current authenticated publication.

Cursor plus HeaderBudget fits 1 KiB; Retained fits 768 bytes. Output
storage, original backing, generated metadata windows and collection
cells remain separately caller-owned/admitted. No additional allocation
or borrowed arena is introduced. Eight units and eight compile-fail guards
cover exact/spare and short windows, selected labels, original custody,
bare-composer bytes/turns/five-cost parity, constructor and
partial-prefix deadline precedence, premature completion, inert complete
polls, explicit/consuming freshness and sticky wire refusal.

Eight sequential Rust allocation intervals cover near-u64 SOURCE
combined tree/list retention; both selections NONE with zero output
capacity; actual expiry after copied bytes; complete Cursor expiry;
complete Retained expiry; released Serialized explicit expiry;
zero-capacity refusal; and constructor actual expiry. Source matching,
complete selected metadata, collection cells and output window are cold;
hot descriptor teardown is counted while pooled scratch/cells and
enclosing cleanup remain outside. All sixteen snapshots must be valid
before comparison. Earlier 185 intervals remain separate. Other metadata masks/sources/capacities, supplied-Tick expiry,
wire/funding refusal, premature finish, cached complete polls, explicit
Cursor/Retained checks and other freshness boundaries remain
allocation-unqualified. Native allocation, stack/RSS and full worker
admission remain unqualified. The fixed subParts scaffold is provisional;
configurable headers/header:*/subParts selection, request bodyProperties
parsing/defaults and current authenticated publication remain later.

### 1.143 Decoded body-part property names

M06el adds `body_property::Cursor` over one immutable decoded request
key. Recognize the twelve standard EmailBodyPart names as distinct Field
values: partId, blobId, size, headers, name, type, charset, disposition,
cid, language, location and subParts. Matching is case-sensitive.
Delegate parameterized `header:NAME[:asFORM][:all]` to §1.28's existing
selector in BodyPart context, preserving the original borrowed key/name,
capitalization, forms, occurrence policy and malformed/forbidden errors.
Email convenience aliases are not body-part properties. Unknown names
return Complete(None) for the enclosing dispatcher to reject; never
reinterpret them as ALL.

A constructor borrows the key and performs no work. Each poll delegates
one existing bounded header turn or prepays one standard-field table
comparison. After twelve misses, a separate terminal turn spends one
record and zero byte visits before returning Complete(None). Header turns retain §1.28's at-most-184 byte visits and 32
records; standard turns spend one record and at most eleven byte visits,
with no output/unlink charge. Different-length comparisons spend no byte
visits. Unknown long non-header keys inspect only the existing bounded
prefix and constant table, with no rescan or second key copy. Work
refusals are sticky across fresh meters; completed recognition is an
inert passive result, not a freshness or publication capability. Error
variants and their Display text reuse the header selector: standard-row
work/state errors also say header property; no new error ABI is added.

Cursor is non-Copy/non-Clone and fits 192 bytes; Property fits 64 bytes.
Eight units and two compile-fail guards pin literal field tags,
case/unknown/alias rejection, borrowed header spelling and exact
header-selector turn/cost parity, malformed/forbidden forms, independent
standard comparison debits, exact byte/record limits, deadline phases,
long header loans and constant-cost long unknown keys.

Eight sequential Rust allocation intervals cover all twelve standard
names, a 64 KiB parameterized header name, a long unknown Unicode key,
malformed suffix, forbidden form, standard-row record refusal,
header-name byte refusal and standard-row deadline refusal. Long keys
and refusal-state preparation are cold. No native allocation, stack/RSS,
whole request admission or authenticated publication claim follows.
Earlier 193 intervals remain separate. JSON decoding and argument-shape
validation, absent/null defaults, duplicate handling, complete
bodyProperties aggregation, configurable header/subParts rendering and
selection-to-composition wiring remain later. Recognition of
headers/subParts does not change §1.140–1.142's provisional structural
scaffold.

### 1.144 Aggregate decoded body-part properties

M06em adds `body_properties::Cursor` over one immutable borrowed decoded
key array and fixed caller-owned header cells. Reuse §1.143's BodyPart
recognizer for every key; no preclassified Email header alias bypass is
accepted. Fields preserve ten independent metadata flags plus headers
and subParts. Repeated standard names are idempotent while each key is
still paid. Unknown keys and malformed/forbidden header forms refuse the
whole selection and retain exact sticky errors across fresh meters.
No whole value is exposed from a partially validated array.

Retain distinct parameterized headers in first-request order, borrowing
the original key/name/forms/occurrence. Deduplicate exact requested key
bytes, keeping the first loan. Differently capitalized or spelled keys
remain distinct response keys. Compare at most 64 bytes from each side
per poll, prepaid as at most 128 byte visits and one record. Different
lengths spend zero visits and one record; insertion spends one record
and overwrites only the selected cell prefix. Capacity refusal remains
sticky and yields no whole value. Reusing filled cells ignores old
content outside the new prefix; spare cells remain untouched.

The deduplication scan is charged quadratic work, not a linear-time
claim. For N header requests, H distinct keys and maximum key length L,
comparison turns are O(N H ceil(L/64)) and visits O(N H L); H distinct
equal-length keys walk H(H-1)/2 pairs. Cell capacity and work quotas bound
that total. Reuse requires a common key-loan lifetime: stale tail cells
retain earlier loans. Reconstruct cells after those loans end before
reusing storage with a different request arena.

View has private fields and read-only fields()/headers() accessors, so
only complete cursor output constructs it. It remains passive data.
Unknown keys return InvalidProperty; malformed/forbidden forms retain
Recognition(parser error). All work stops, including comparison and
insertion, use Recognition(Work(stop)) and the existing recognition
Display beneath the selection prefix. A future RPC dispatcher must
classify unknown/malformed/forbidden names as invalidArguments; this
increment supplies no RPC error mapping.

Parsing polls preserve §1.143's bounds. Applying one recognized field or
starting header deduplication is bounded and shares that recognition
turn's payment; no rescan or second key copy occurs. Explicit empty
arrays are immediately complete NONE with zero work and zero cell
capacity. Completed selection is an inert passive value, not job
freshness or publication authority. Consuming finish requires complete
recognition and returns the same borrowed cell prefix. Cursor is
non-Copy/non-Clone and fits 384 bytes; each Cell fits 64 bytes. Keys,
array and cells remain separately caller-owned/admitted.

The pure Fields::DEFAULT constant names the ten metadata fields only,
with headers and subParts false, matching the omitted bodyProperties
argument in [RFC 8621 §4.2](https://www.rfc-editor.org/rfc/rfc8621.html#section-4.2).
It does not reinterpret explicit empty or unknown arrays as a default.
JSON argument decoding/type checks, absent/null handling and actual
selection-to-composition wiring remain later. The existing compositor's
provisional subParts scaffold is unchanged; carrying a structural flag
here does not claim configurable structural/header rendering.

Nine units and three compile-fail guards pin all literal choices,
default/explicit-empty separation, passive cached completion, exact-key
header deduplication and first borrowed loans, per-key debit parity,
unknown/forbidden refusal, prefix/tail/capacity behavior, every deadline
cut, exact/one-short work limits, premature finish and long two-sided
comparisons. Eight sequential Rust allocation intervals cover all fields,
long duplicate header keys, explicit empty cached completion, unknown
key, forbidden form, header-cell capacity refusal, duplicate comparison
record refusal and duplicate comparison deadline refusal. Long keys and
refusal-state setup are cold; all sixteen snapshots must be valid.
Earlier 201 intervals remain separate. Other masks/keys/forms, supplied
work limits, premature completion and repeated cached polls remain
allocation-unqualified. Native allocation, stack/RSS, full request
admission and current authenticated publication remain unqualified.

### 1.145 Decode a bodyProperties JSON argument

M06en adds body_properties::json::Cursor over an already UTF-8 admitted
argument fragment, fixed caller-owned key-reference slots and cells
borrowing caller-preallocated Strings. Cold Cell::new clears each String;
no allocator growth is allowed during polls. The caller admits input,
String capacities, cells and reference slots separately. Consumed cells
must be reconstructed once all decoded loans end before another request.

Only None denotes the omitted argument and returns passive Default with
zero work. Explicit null and non-array/non-string values refuse Syntax.
Explicit [] returns Explicit with an empty prefix, preserving NONE rather
than DEFAULT when passed to §1.144. Decode complete array syntax and EOF,
accept ASCII JSON whitespace and standard escapes, and reject trailing
commas/data, controls, malformed escapes and unpaired UTF-16 surrogates.
Validated raw UTF-8 and surrogate pairs emit whole scalars only. Unknown
property strings decode successfully; the original BodyPart recognizer
still owns semantic validation. No JSON object/dispatcher is provided.

Each nonterminal poll prepays one source-octet visit and one record; raw
non-ASCII scalar decoding additionally prepays its full two-to-four-byte
width for chars().next() plus one leading-octet visit for the str::get
boundary check. Thus a poll spends at most six input visits, one record
and four output bytes.
Output bytes are paid before checking remaining actual String capacity
and copying a scalar. A failed capacity check may retain that payment,
but never grows the String. Hex digits are separate paid turns; a pair
emits only after the complete validated low surrogate. EOF spends one
record and zero visits, including the successful terminal poll. There
is no final source/key rescan. Long ASCII input therefore costs its
source length in visits and source length plus one records, plus the
decoded text length in output bytes.

Only complete value/finish exposes the selected key prefix, borrowing
original String allocations without a second key copy. Partial String
prefixes and completed earlier slots survive refusal; no whole argument
is exposed. Malformed JSON/escapes return Syntax; impossible UTF-8 lead/slice
states return InvalidState. Work, Syntax, Capacity and InvalidState
errors are sticky across fresh meters. Completed polls are inert passive reads, including
past a deadline; they confer no job freshness/publication authority.
Cursor is non-Copy/non-Clone within 256 bytes; each cell fits 16 bytes.

Nine units and two compile-fail guards cover omitted/null/empty
separation, independent escape/scalar literals and exact work debits,
all whitespace positions, lowercase hex, separator grammar,
invalid syntax, actual UTF-8 capacity boundaries and original pointers,
every deadline cut, exact/one-short input/record/output budgets,
premature finish, escaped names through §1.144 and a 64 KiB ASCII header
without a terminal scan. Eight sequential Rust allocation intervals
cover omitted cached completion, explicit empty, long ASCII plus escaped
pair, lone high surrogate, actual text capacity, isolated record refusal,
isolated scalar output refusal and partial-hex deadline refusal. All
sixteen snapshots are validated; String/source/refusal-state setup is
cold. Earlier 209 intervals stay separate. Other input shapes, supplied
limits and repeated cached reads remain allocation-unqualified. Native
allocation, stack/RSS, complete request UTF-8 validation/admission,
selection-to-composition wiring and current authenticated publication
remain unqualified.

### 1.146 Decode and select one bodyProperties argument

M06eo adds body_properties::request::Cursor, a facade joining §1.145's
JSON argument decoder with §1.144's semantic selection over the same
caller-owned text cells, decoded-key slots and header cells. None is
immediately complete with the literal ten-field DEFAULT and an empty
header prefix. Explicit [] remains complete NONE after its paid JSON
EOF. Null, unknown names, Email aliases and malformed/forbidden header
forms refuse whole success; the original parsers own each error.

Each poll executes one original JSON or semantic-selection poll, with
identical debits and bounds: at most 184 input visits, 32 records and
four output bytes. After successful paid JSON EOF, finish the decoder
and transfer its borrowed key prefix into the original selector without
copying or scanning keys. An empty selector is immediately complete;
otherwise recognition begins on the next turn. There is no extra
transition fee or hidden second parser poll. Original exact key spelling,
first duplicate header loan and selected cell prefix survive.

Only complete semantic value/finish exposes the private-field View;
JSON-only success is insufficient. First Json(error), Selection(error)
or InvalidState remains exact and sticky across meters. Incomplete
finish refuses InvalidState. Cached complete polls are passive/inert,
including after deadline, and confer no job freshness/publication
permission. Default fills no cells and consumes no work. Cursor is
non-Copy/non-Clone and fits 512 bytes. Text capacities, key slots and
header cells remain separately admitted cold storage; all decoded loans
and stale tail lifetime rules in §1.144–1.145 still apply.

Ten units and two guards pin literal default/empty/null choices,
escaped metadata/structural fields and first header pointers, complete
per-turn parity with both original stages, exact sticky syntax/capacity
and semantic errors, every deadline cut, exact/one-short input/record/
output limits, premature finish, cached complete views and a 64 KiB
header loan. Eight sequential Rust allocation intervals cover paired
omitted/explicit-empty completion, all twelve fields, long duplicate
headers, null syntax refusal, unknown semantic refusal, header capacity
refusal, isolated duplicate-comparison record refusal and semantic-start
deadline refusal. All sixteen snapshots must be valid; caller text,
source and refusal-state preparation stay cold. Earlier 217 intervals
remain separate. Other masks/shapes/limits, incomplete finish and
repeated cached polls remain allocation-unqualified. Native allocation,
stack/RSS, whole request framing/admission, configurable composition
wiring and current authenticated publication remain unqualified.

### 1.147 Select subParts in requested selected composition

M06ep adds mime_response::subparts_json
under §1.141's original source-bound custody. Its pure Selection combines
immutable outer Properties with an explicit sub_parts flag; selected
metadata keeps its existing ten-field label. Original Serialized, rather
than passive views or advanced composers, is the constructor input.

When sub_parts is false, close each selected BodyPart after its metadata.
A bodyStructure contains only the original root and does not descend;
textBody/htmlBody/attachments still use the original leaf classifications.
Empty metadata gives {}. When true, preserve the existing complete tree
and leaf-null framing byte-for-byte, turn-for-turn and debit-for-debit.
The shared Frame owns both paths. Original collection validation and all
caller buffers remain admitted before construction. No source traversal,
classification, pointer or fresh ownership is reconstructed from output.

Selection survives passive values, consuming completion and release of
the exact original selected Serialized. Original constructor, unfinished
poll, explicit deadline check, consuming finish and release enforce both
job and pin deadlines. Complete polling remains inert; an explicit failed
check hides values stickily. No passive value authorizes publication.
Keep one aggregate step per turn and one job record every sixteen turns,
zero source/input visits and at most 64 prepaid wire bytes. Zero-capacity
output is unpaid; outer NONE is immediately complete but construction/release
remain fresh. Cursor plus HeaderBudget and 64-byte output fits 1 KiB;
Composed fits 768 bytes. Both owners are non-Copy/non-Clone.

Nine units and seven compile-fail guards cover independent literal JSON,
root-only/list output, immutable selection and original pointers, legacy
true-path routing parity backed by independent bytes, a three-node nested
tree, bounded output widths, every false-path deadline cut,
fresh construction/completion/release, sticky checks and premature finish.
Eight Rust allocation intervals cover false root/tree plus lists, outer
NONE, selected multipart tree composition, expiry after copying, consuming
completion expiry, composed release expiry, zero-capacity output and
constructor expiry.
Caller collection/file/fragments and constructor-refusal setup stay cold;
expiry transitions after copying/completion remain counted. All sixteen
snapshots must be valid before comparison. Earlier 225 intervals remain
separate. Other shapes/masks/limits and native allocation, stack/RSS,
header/header:* rendering, full request-to-composition admission and
current authenticated publication remain unqualified.

### 1.148 Retain whole selected subParts composition

M06eq adds §1.147's mime_response::subparts_window facade over the original shared
fixed Window and retention core. Only the original selected Serialized
may begin fresh whole retention. Advanced cursors, completed emitted-only
owners, passive views and unselected collections cannot recover missing
prefixes. Immutable outer/structural Selection and selected metadata
survive complete passive values, consuming retention and original release.

Poll uses the same framing turn and prepaid five-domain costs as streamed
composition; no extra transition fee or classification walk is added.
Exact output capacity completes; shorter windows return the original
ResponseCapacity stickily and expose no whole bytes. Spare output remains
untouched. Root-only output when subParts is omitted and original tree
output when selected retain §1.147's byte semantics. Outer NONE succeeds
in an empty window without work; constructor and consuming owners remain
fresh in both job/pin domains. Original completed polls are inert, while
explicit checks and consuming completion/release still enforce deadlines.
Cursor plus HeaderBudget fits 1 KiB; Retained fits 768 bytes. The caller's
output window remains separately admitted cold storage, not cursor state.

Eight units and eight compile-fail guards pin complete byte/selection/
pointer custody, a literal root-only empty-metadata oracle, exact/short
windows, fresh capacity precedence, post-copy expiry, all completed-owner
fresh checks/releases, outer NONE and per-turn streamed debit parity.
Eight Rust allocation intervals cover omitted root/lists, outer NONE,
selected multipart tree, copied-prefix expiry, consuming completion
expiry, Retained release expiry, short capacity and constructor expiry.
Original collection, file, output and refusal preparation stay cold.
Clock expiry transitions in the three post-construction refusals remain
inside the measured intervals.
Validate all sixteen snapshots before comparing pairs; earlier 233
intervals remain separate. Other shapes/masks/limits, native allocation,
stack/RSS, header/header:* rendering, full request admission/wiring and
current authenticated publication remain unqualified.

### 1.149 Resident source-ordered header arrays

M06er adds mime_headers::json::Cursor over one caller-authorized immutable
resident Input, including original base, header limit and EOF/prefix
knowledge. Reuse Scanner's tolerant recognized-field/body-boundary policy;
this is not a stricter replacement parser. Actual EOF completes an
unterminated final field; an exhausted prefix without a recognized boundary
refuses Truncated. Preserve original field order, duplicate names and name
case. Emit EmailHeader objects with name and value members, using the
existing Raw scalar and JSON string engines. Keep colon-following whitespace
and embedded folds, exclude the terminal line ending, drop NUL through Raw,
and retain its existing UTF-8/noncharacter replacement and diagnostics.
Do not interpret encoded words or normalize combining sequences.

Each caller-output fragment is provisional until whole healthy completion.
The cursor retains original job/HeaderBudget borrows across exclusive scan
and Raw phases. It maps only original scanner extents into that same Input.
No field-byte vector, header table, decoded string, growing output or second
scanner is created. Each scan turn visits at most 255 bytes and charges at
most 256 interpretation steps; string turns reuse the existing bounded Raw
and six-byte JSON frame. Literal copies are at most 64 prepaid bytes with
one interpretation step per nonempty turn; each name/value begin turn also
pays one interpretation step. Raw and coordinator credits stay
private and non-replayable. The original source visits, interpretation steps,
job records and wire bytes remain charged; no unlink operation is added.
Zero-capacity output checks fresh admission without advancing or paying work.

Constructor, every unfinished turn, explicit checks and consuming finish
check the supplied job deadline and live interpretation admission. Complete
polls remain inert. Refusals retire the owner, hide completion and remain
sticky. Fresh finish hands back the exact original Meter/HeaderBudget with
passive End and encoding diagnostic; it cannot recover emitted output or
provide source/pin/publication authority. The caller must bracket turns with
fresh clock/cancellation and original pin checks. Cursor plus HeaderBudget
fits 512 bytes; Completion fits 64 bytes; caller output is separate storage.

Nine units and four compile-fail guards pin literal ordered Raw JSON, source
EOF/prefix and tolerant boundary semantics, replacement/filtering/escaping,
every incomplete prefix, offset/header limits, zero-capacity/prepaid wire
refusals, every unfinished deadline cut and completed admission, exact budget
handoff, exact simple-field whole-run charges, budgeted Raw replay refusal
and bounded scans/interpretation/job refusals. Eight Rust allocation
intervals cover empty EOF, folded ordered headers, malformed Raw bytes,
incomplete prefix, wire refusal, header-limit refusal, unfinished expiry and
completed release expiry. Source/budget/output preparation stays cold;
constructor, polling and consuming refusals remain measured. Validate all
sixteen counter snapshots before comparing pairs. Earlier 241 intervals stay
separate on the accepted retention parent. M06es below qualifies whole-array
fixed-window retention. Configurable body-part header composition, original
pin/admitted request
integration, authenticated publication, native allocation and stack/RSS
qualification remain later.

### 1.150 Whole resident header-array retention

M06es adds mime_headers::json::retained::Cursor. Construct its own fresh
resident header-array cursor from the original caller-authorized Input and
exclusive original Meter/HeaderBudget, plus one distinct caller-reserved
fixed Window. An advanced streamed cursor or emitted-only Completion cannot
start whole retention. Reuse the original bounded scanner/Raw/JSON turns and
all charges. The window records only reported writes without growing or
reconstructing a lost prefix. Exact capacity completes; short capacity refuses
ResponseCapacity stickily, hides whole output and retires the original owner.
All copied bytes remain provisional until healthy whole completion.

Fresh original admission precedes capacity refusal, including empty or full
unfinished windows. After healthy admission, a full Window refuses capacity
before the next producer turn, even if that turn would write no bytes and
reveal an incomplete source. For prefix A:a CRLF B:b, a window ending exactly
after the first object refuses ResponseCapacity; one spare byte permits the
scanner turn and reveals Truncated. Neither refusal refunds work already paid,
and this owner cannot grow its window or restart an emitted prefix.

Completed polls remain inert. Construction, unfinished turns, explicit checks
and consuming finish retain supplied job deadlines and original interpretation
admission. An explicit failed check hides completed values. Fresh finish
returns passive Retained bytes, End and encoding diagnostic together with the
exact original budget Completion. Neither passive value grants original pin,
source or publication authority; the caller still brackets turns with fresh
clock/cancellation and original pin checks. Retained bytes occupy only the
original output identity and leave spare capacity untouched. Cursor plus
HeaderBudget fits 640 bytes; output storage remains separate and cold.

Seven units and four compile-fail guards pin literal ordered/case-preserving
Raw JSON, exact/spare and every short window, empty/encoding diagnostics,
original prefix/wire refusals and capacity/source ordering, all unfinished
deadline cuts including full windows, exact-window completed polls,
checks/release, expired premature finish, budget pointer handoff and
streamed byte/cost
parity. Eight Rust allocation intervals cover empty EOF, ordered folded Raw,
malformed Raw, incomplete prefix, short window, constructor expiry,
empty-window unfinished expiry and completed release expiry. Original
source, budgets and
fixed output are prepared cold; construction/polling/consuming refusals are
measured. Validate sixteen snapshots before comparing pairs; earlier 249
intervals remain separate on the accepted streamed-header parent. Other header
selection/forms, original pin/full admitted request wiring, authenticated
publication, native allocation and stack/RSS remain unqualified.

## 2. Read views and change history

store_fs::IndexStore owns a locked root, one serialized SQLite writer and
one to eight cold reader connections. create requires a fresh database;
open validates the physical files and exact schema before accepting metadata.
Both borrow LockedRoot exclusively for the owner's lifetime, preventing
independent owners from bypassing its checkpoint/collection fence.

view captures a real SQLite WAL read transaction under the writer mutex.
ViewIdentity contains account, epoch, committed_sequence and history_floor.
Later commits remain invisible. get and next decode bounded typed rows into
caller buffers; next follows strictly increasing encoded keys. Missing rows
return None; insufficient buffers return Capacity. Bad rows return Corrupt.
Detached rows borrow the caller buffers and confer no body or view ownership.

next_change reads the native kind/sequence/operation index through the captured
endpoint. Below-floor cursors return HistoryLost; above-end cursors return
Invalid. Matching entries retain action and operation order. The SQL adapter
returns Record or Complete; it needs no replay-only Advanced step. Identity
changes are refused. A view retains its original monotonic deadline and VM
fuel; clock/VM-budget failure or reversed time is sticky. Drop ends the read transaction
before returning its connection; failed rollback retires the slot.

IndexReadView::open_blob_input takes the crypto provider, blob ID and byte
ceiling. The snapshot supplies the BlobRow, account, original clock and
deadline. Complete length/digest/EOF verification yields PinnedBlob random
reads from the same file. The input and completed pin borrow the view, which
blocks slot release and orphan collection until that borrow ends. A caller
cannot renew the deadline by supplying another clock or duration.

checkpoint takes the writer fence and refuses while any view is held. A
successful SQLite WAL TRUNCATE plus root sync permits further bounded writes.
collect_orphan requires a durably absent current BlobRow, no live views,
validated immutable path, unlink and parent sync. The core does not implement
an online backup, complete orphan walk, history-pruning scheduler or runtime
slot arbitration; operational callers must provide those admission policies.

## 3. Reserve, publish, commit

The existing ports::Store trait is the future authenticated service boundary;
IndexStore is its low-level persistence core, not an implementation of device
authorization or the complete reservation coordinator. Access still requires
account, trusted principal and current configuration. Recheck revocation and
permissions at commit. Named SMTP, queue and maintenance principals retain
only their authorized operation rights. Queue outcome recording uses its
durable attempt fence even if credentials change after transmission.

Reserve body, operation, response and category capacity before accepting
responsibility. ReservationId retains coordinator instance, slot and checked
generation. Linear logical effect tickets prevent double reconciliation;
physical I/O can still fail. Published but unreferenced files stay charged
until durable cleanup, independently of unused logical reservations.

IndexStore::commit accepts Crypto, CommitRequest, a bounded Operation slice
and owner-bound PublishedFile proofs. CommitRequest carries account, expected
sequence, UTC validation time and one monotonic deadline. Every fresh BlobRow
requires a matching durably published, exact-length and digest-verified file.
A permanent SQLite ID registry prevents recreating deleted blob IDs. No
message bytes are copied into database values or transaction staging.

The serialized transaction checks the expected sequence, applies operations
in caller order, validates final owning references and parent chains, records
changes and updates the unsigned account sequence. Change actions agree with
pre/post existence; a native unique index refuses duplicate object changes.
Repeated row keys take their last effect. Caller authorization, queue
transitions, aggregate recipient policy and complete changed-object coverage
remain the coordinator's responsibility.

COMMIT with synchronous FULL precedes success. CommitError::Rejected means
the pre-COMMIT check or a deferred-constraint/busy refusal rejected the
transaction and rollback completed; failed rollback additionally retires
writes. A successful FULL COMMIT plus autocommit reports its durable sequence
even when the deadline expires during completion. Other COMMIT errors are
Indeterminate and stop writes until reopen/recovery. New read snapshots
remain available. Never report an ordinary retryable method error for an
uncertain external effect. Response notification can fail after durable
commit; the service still needs idempotence/result-retention reconciliation.

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
maxChanges across their combined size. Page only at complete transaction boundaries;
choose the latest scanned boundary that fits all effects, pinning that page's
old/new state. If no advancing boundary fits before the work budget expires,
return cannotCalculateChanges. Do not split a transaction or omit excess IDs.
hasMoreChanges means more transactions remain through the pinned target; the next
request may see a newer target. An unchanged request at current returns empty
arrays, equal states and false. Retention loss forces explicit resynchronization.

V1 queries return canCalculateChanges=false; /queryChanges returns the standard
cannotCalculateChanges for supported well-formed queries. They do not rerun a
query and pretend its changed ordering is a delta. POLICY.md specifies the
queryState encoding and bounded sort/search semantics; CASES.md names the
wire fixtures. The queryState codec is still owned by M14/M16.

Identity data comes from one atomically selected validated configuration
snapshot, outside the mail database. Digest preimage is ASCII td-mta-identities-v1
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
Reserve response capacity before mutation. An indeterminate SQLite commit outcome
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
