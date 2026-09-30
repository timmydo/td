# TLS configuration and session contract

## Status and ownership

This is the M07 contract for the public TLS facade. ClientConfig,
ServerConfig and the shared ClockHandle are implemented; public sessions
remain future work. Bounded PEM syntax, P-256 PEM key loading, local
ServerIdentity admission and TrustStore construction are implemented.
Admitted identities retain a private TLS signer sharing the same owned
key and retirement state. The private algorithm provider implements the
fixed suite/group/signature lists below; configurations apply their
fixed policies and immutable name routing. DESIGN.md records implemented
acceptance and output contracts. The existing private TLS fixtures
qualify selected backend behavior only. Implementations must satisfy
this contract and the resource/failure qualification in DESIGN.md before
td-mta enables a listener or an outgoing connection through them.

All public types are td-owned and expose only std or td-crypto types.
Backend configuration, certificate, verifier, error and connection objects
remain private. There is no runtime backend selector. td-crypto performs no
socket, filesystem, DNS, logging or mail-policy operation. td-mta owns those
operations, trusted input loading, resource leases, deadlines and
configuration publication. The shared facade neither grants gateway
authorization nor selects mail routes.

The facade has immutable, shareable `ServerConfig` and `ClientConfig`
handles and one exclusively owned `TlsSession` per connection.
Configurations retain parsed material; sessions retain their configuration
until dropped. Handles are Send; configurations are Sync and sessions permit
only exclusive mutation. Creating/dropping sessions and backend work may
allocate within the separately qualified TLS allowance. These are not
allocation-free crypto operations.

## Fixed protocol and algorithm policy

The baseline enables TLS 1.3 followed by TLS 1.2. Earlier versions,
renegotiation, record/certificate compression, QUIC, PSK-only
authentication, post-handshake client authentication and early data are
unsupported. Disable client resumption, server session storage, stateless
ticket production, initial TLS 1.3 tickets and on-request TLS 1.3 tickets
explicitly. Every new connection performs a full certificate-authenticated
handshake. Disable half-RTT application data, TLS key logging and secret
extraction. No environment option enables these facilities. TLS 1.3
KeyUpdate is bounded record work, not renegotiation. Require extended master
secret for TLS 1.2 ([RFC 7627
§5.2](https://www.rfc-editor.org/rfc/rfc7627.html#section-5.2)).

Set both certificate compressor/decompressor lists empty and the compression
cache to Disabled on both configurations; a default empty algorithm list does
not eliminate the default cache. Use the non-ECH builder path, enable outbound
SNI and selected-ALPN validation, and leave the client ticket-request extension
disabled (`send_ticket_request = None`). Pin these choices in configuration
inventory tests; no provider-default change may silently enable them.

The cipher suites, in configured preference order, are:

| Version | Suites |
| --- | --- |
| TLS 1.3 | AES_256_GCM_SHA384; AES_128_GCM_SHA256; CHACHA20_POLY1305_SHA256 |
| TLS 1.2 | ECDHE_ECDSA_WITH_AES_256_GCM_SHA384; ECDHE_ECDSA_WITH_AES_128_GCM_SHA256; ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256; ECDHE_RSA_WITH_AES_256_GCM_SHA384; ECDHE_RSA_WITH_AES_128_GCM_SHA256; ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 |

The table omits the standard TLS_ prefix. This list admits only
forward-secret ECDHE and AEAD suites. Prefer the server's configured
cipher-suite order. Clients offer groups in the order X25519, secp256r1,
secp384r1. Servers accept exactly that set but select according to the
client's group order under the pinned backend; cipher-suite preference does
not establish server group preference. Post-quantum and hybrid groups are
outside this initial qualification scope. Their additional handshake/native
memory and stack paths remain unqualified, not proven too large. This
deliberately supplies no protection against future quantum decryption of
recorded traffic. Adding hybrid support needs its own resource and
interoperability qualification, though the existing dependency can implement
it. Disabling the provider's preference feature does not remove its hybrid
fallback; set and test the accepted groups explicitly.

Certificate signatures may use ECDSA with P-256, P-384 or P-521 and SHA-256,
SHA-384 or SHA-512; Ed25519; or RSA with 2048..8192-bit keys and SHA-256,
SHA-384 or SHA-512 using PKCS#1 v1.5 or PSS. RSA PSS here uses rsaEncryption
public keys, not restricted RSASSA-PSS SubjectPublicKeyInfo. Require MGF1
with the same hash, salt length equal to the digest length and trailer field
one. Accept the pinned backend's RSA PKCS#1 signature identifiers with
absent or NULL parameters. No SHA-1, DSA, Ed448 or post-quantum signature
algorithm is admitted. The provider defaults also include ML-DSA in both
verification lists: excluding hybrid key exchange does not remove those
signatures. Trust anchor self-signatures are not path signatures and need
not be reverified; anchors remain explicit trusted configuration, not
verified peer identities.

Handshake signature scheme preference is ecdsa_secp384r1_sha384,
ecdsa_secp256r1_sha256, ecdsa_secp521r1_sha512, ed25519,
rsa_pss_rsae_sha512, rsa_pss_rsae_sha384, rsa_pss_rsae_sha256,
rsa_pkcs1_sha512, rsa_pkcs1_sha384, rsa_pkcs1_sha256. The last three are TLS
1.2 only. TLS 1.2 ECDSA scheme hash selection does not constrain the signing
curve; TLS 1.3 requires the named curve. Certificate-path verification and
handshake-signature verification have separate pinned algorithm sets.
Neither inherits additional algorithms when the provider defaults change.
Pin each scheme-to-algorithm mapping and its internal order too: TLS 1.3
uses only the first verification algorithm mapped to a scheme.

These are accepted algorithms, not an assertion that every intersection of
key, version and suite can negotiate. Exhaustive policy inventory tests plus
representative positive and excluded-algorithm negative fixtures are
required. Existing P-256-only loopback fixtures do not prove the complete
inventory.

## Material and configuration admission

Admit material on the cold control path from borrowed byte slices, then
construct configurations from admitted handles and a shared clock.
Constructors retain no caller slice; success owns parsed material and
failure publishes no usable configuration. The caller charges input,
temporary, parsed, shared and overlapping allocations to its generation
reservation. There is no implicit file read, OS certificate store,
certificate download, trust cache or fallback to an older configuration.

A local identity consists of a leaf-first PEM certificate chain and a
separate unencrypted PKCS#8 PEM private key. Accept CERTIFICATE and PRIVATE
KEY labels only, LF or CRLF, standard padded base64 and ASCII whitespace
between blocks. Refuse mixed labels, unknown text, extra key blocks, empty
chains, malformed base64, trailing DER, encrypted keys, PKCS#1/SEC1
standalone keys and PEM headers. Apply byte ceilings before decoding: chain
64 KiB, key 16 KiB, explicit trust bundle 128 KiB. Limit a presented/local
chain to eight certificates, each certificate DER to 16 KiB and their
aggregate DER to 64 KiB. An explicit trust bundle contains at most 128
certificates. Decoding must not allocate based only on an unchecked input
length.

For the initial local signing identity accept P-256 keys in exactly the
PKCS#8 subset already defined in DESIGN.md, wrapped by PRIVATE KEY PEM.
Managed ACME uses that same representation. This local restriction does not
restrict the peer key algorithms above; a server with a P-256 identity
negotiates ECDSA TLS 1.2 suites. Adding other local key formats is a
compatibility increment, not permission for the loader to try arbitrary
backend parsers. Input keys and temporary decoded copies follow the
secret-lifecycle contract; ordinary buffer clearing supplies no universal
erasure claim.

Require key/leaf agreement, a non-CA leaf usable for server authentication,
valid dates for every supplied chain certificate at admission, supported
key/signature algorithms, and SAN coverage of every configured exact DNS
name. When KeyUsage or ExtendedKeyUsage is present it must permit the
operation; absent usage extensions follow normal X.509 verification
semantics. Validate chain ordering and each supplied issuer
relationship/signature; reject duplicate certificates. An omitted root is
permitted. This consistency check does not establish trust in the last
certificate: remote peers apply their own roots. Private deployments may use
an explicitly configured self-signed server leaf with the same key,
validity, usage and name checks. Do not substitute a generated self-signed
certificate.

Local identity admission additionally limits each certificate to 64
extensions and the backend's X.509 v3/1970-or-later date subset. It requires
an uncompressed P-256 leaf point and canonical owned metadata encodings;
DESIGN.md specifies the exact subset and partial-chain signature limits.
Local admission alone does not establish remote trust.

A server configuration contains at most 16 identities and 512 exact DNS-name
bindings, with at most 32 names per identity and 253 ASCII bytes per name.
Names are validated, folded ASCII DNS names without a trailing dot;
configured wildcard route keys, IP literals and duplicate/conflicting
bindings are refused. A certificate's valid wildcard SAN can cover an exact
configured name under ordinary certificate name-validation rules. Store a
checked local identity index, never an application pointer or mail
identifier, in the routing table.

Selection policy is explicit and cold-selected by the adapter. HTTPS
requires matching SNI and refuses missing/unknown SNI. A direct-SMTP
configuration has one identity and uses it for absent, matching or
unrecognized well-formed SNI; an unnecessary SNI refusal could drive
opportunistic senders to retry in plaintext. A gateway-SMTP
configuration also has one identity and permits absent SNI, but refuses
present SNI that does not match a configured name. Malformed SNI remains
a protocol error. The pinned backend converts raw IP-literal SNI to
absence before lookup. The session facade must validate raw SNI before
that information is lost, including fragmented and retry hellos; the
configuration resolver alone cannot enforce this refusal. These are
generic selection modes in the facade, not imported mail-role types. The
concrete IdentitySelection modes are RequiredName for Http1,
DefaultIdentity or MatchPresentName for Smtp. Other combinations are
refused. SNI is a routing claim, not client authentication. The mail
adapter also validates HTTP authority against its listener/name policy.
Neither SNI nor a certificate authorizes JMAP.

Server client authentication is either disabled or mandatory with an
explicit private trust bundle. There is no optional mode.
CertificateRequest omits CA-subject hints to bound its length; the
complete configured anchor set still verifies every client. With
mandatory authentication, verify chain, dates and client-auth usage
before accepting Finished; no public root fallback is allowed. No AIA,
CRL or OCSP network fetch is performed; v1 does not claim online
revocation checking. Gateway revocation is applied by td-mta's current
pin/address policy and configuration-generation rules. Client
configurations use either the pinned public root set or one explicit
bundle that replaces it completely. Invalid/empty explicit trust is an
error. Initial explicit stores admit the CA-only, unconstrained-anchor
subset specified in DESIGN.md; they refuse anchor EKU, path length and
name constraints rather than silently discarding them. Public roots
retain their upstream anchor constraints. Verify server chain, dates,
usage and the supplied DNS name; send that name as SNI. V1 outbound
mail/ACME has no client identity. The facade need not expose
client-certificate signing until a real consumer requires it; test-only
mutual peers are not public API consumers.

HTTP/1.1 clients offer only `http/1.1` ALPN, and HTTPS servers select only
it; absence is accepted for HTTP/1.1 compatibility. Refuse a negotiated
unknown protocol. SMTP configurations offer/select no ALPN. The facade
receives a fixed td-owned protocol selector, never an arbitrary ALPN list.
No h2 engine or ACME TLS-ALPN-01 protocol is implied.

A td-owned shared Send + Sync clock trait returns optional UTC seconds since
Unix epoch. Its callback performs bounded, nonblocking work; configurations
retain a shared clock handle and never retain a short-lived caller borrow.
ClockHandle owns and serializes the source. Missing time returns Clock and
leaves the source available for other operations; callback unwind permanently
retires the shared source and later calls return Crypto. A poisoned handle
also refuses. Callbacks must not reenter the handle. DESIGN.md defines the
consuming unwind boundary and its limits.
All backend time requests use this clock, with no hidden system fallback.
Missing/unrepresentable time fails Clock, including on an open connection.
td-mta owns monotonic deadlines and clock-health policy. Check local server
material at identity selection and handshake completion, when the selected
identity is known; do not let an expired unrelated SNI identity block
another valid identity within an already admitted configuration. Cold
configuration construction still requires every included identity to be
valid; a candidate with expired or retired material is refused atomically.
Replace or explicitly remove that material before publication; do not silently
omit configured identities. An existing configuration can continue serving
its other valid identities while a candidate is refused. Single-identity
configurations can also check at session creation. Peer verification checks peer dates. Configuration
construction is not a permanent validity grant. Established policy and
publication/revocation remain with td-mta.

Disabling resumption does not eliminate incoming TLS 1.3 ticket processing.
The pinned client still derives a ticket secret, requests time, and copies
the ticket and peer chain before the no-op store drops them. Charge that
transient allocation and established-session work to the TLS session budget,
including several tickets within one record. Test a ticket-sending remote
server with the clock becoming unavailable after handshake completion.
Constructor-only and handshake-only clock tests are insufficient.

TLS 1.2 session saving also requests time after server Finished when the
peer assigned a session ID, despite disabled client resumption. It silently
ignores missing time; with time available it copies the master secret and
peer chain before the no-op store drops them. Charge those temporary copies.
The facade must record each backend time failure in private per-connection
state and inspect it after every backend call, including successful calls.
Check shared clock retirement without another callback before publishing
results. A transient None followed by a successful re-poll must still retire
the affected session with Clock. A callback panic retires the shared source
and requires Crypto, even if the backend reports Finished success. Also check
time on operations where the backend does not request it. The configuration
fixtures pin this backend hazard; session enforcement remains M07c work.

## Session progress and buffers

Construct a session from one immutable configuration; client construction
also receives the checked server DNS name. No credential or application byte
may be accepted before the full handshake completes. A server may expose its
selected identity index for routing only after successful selection, but
this is not peer-authentication evidence. Configurations contain no mutable
session cache.

The socket-free operations are: receive one complete TLS record, drain
pending ciphertext, read received plaintext, queue plaintext, request a
clean close, report transport EOF, inspect progress, and abort. Each
operation returns fixed td-owned progress/errors. No method retains caller
buffers after return. Progress includes input/output readiness and
read/write closure independently of the overall phase, so a caller cannot
confuse clean read EOF with a fully closed connection. Before handshake
completion, application read/write calls return Blocked with no consumption.
Failed sessions return their fixed error; closed reads report EOF and closed
writes fail. Status inspection is always available and never runs backend
work. Implementation freezes concrete signatures with its compiling
interface tests; these operation semantics cannot change silently when
signatures are added.

The caller assembles the five-byte record header and its declared body in a
preallocated wire buffer. `receive_record` accepts exactly one record or
consumes none. The wire reservation remains 18437 bytes, but reject body
lengths >= 18432 to match the pinned backend's strict outer bound. Also
enforce the 16384-byte unencrypted-fragment limit and the negotiated
encrypted-record limits, including TLS 1.3's 16640-byte ciphertext ceiling
([RFC 5246
§6.2.1](https://www.rfc-editor.org/rfc/rfc5246.html#section-6.2.1),
[§6.2.3](https://www.rfc-editor.org/rfc/rfc5246.html#section-6.2.3), [RFC
8446 §5](https://www.rfc-editor.org/rfc/rfc8446.html#section-5)).
Distinguish protection state from handshake completion: TLS 1.2 encrypts
Finished before completion, while TLS 1.3 uses encrypted outer
application-data records. Track validated protocol/ChangeCipherSpec
transitions privately rather than treating every handshaking record as
plaintext. Reject extra records, length mismatch and oversized records
before backend intake. Partial network reads remain caller framing state; no
intake call waits for another record.

The pinned deframer has one 65535-byte retained-buffer ceiling for handshake
reassembly, including retained framing; there is no additional 64 KiB pool.
Its payload-length guard is also 65535, so framing can make a near-limit
payload fail to fit. Do not promise that a 65535-byte payload plus its header
is admissible. Test the exact complete/fragmented boundary behavior and limit
peer certificate count/size before unbounded allocation. An input ceiling
alone does not prove the backend's retained or temporary memory bound.

Intake returns Blocked without consumption when received plaintext must be
drained. During handshake only, drain outgoing ciphertext before accepting
another record. Established sessions continue accepting records while
application output is pending, so both peers can read while writing; reserve
bounded protocol-output headroom for KeyUpdate/alerts separately from the
single pending application record. Exceeding a qualified resource ceiling is
terminal Capacity, never circular backpressure. The TLS 1.2 alert exception
below is a terminal refusal, not a wait for an already-closing peer to read.
Each intake call receives the caller's current indication of
socket-unwritten ciphertext, in addition to the facade's own pending-output
state.

Queueing plaintext accepts at most one 16384-byte chunk and reports the exact
count, or Blocked with zero consumption. Empty application/output slices on
an otherwise permitted operation consume zero without progress. A short
acceptance is legal and promises no socket transmission. Blocked identifies
whether to drain plaintext, drain ciphertext or supply peer input; it cannot
require input while refusing that input in the same state. Drain operations
write at most caller capacity and report their exact count. Unwritten bytes
remain owned by the session. A drained chunk can split a TLS record; the
caller retains any socket-unwritten suffix before draining again and reports
that outstanding suffix to intake. Never regenerate drained bytes.

Pin the exact internal plaintext/ciphertext queue and protocol-output
reserve limits in implementation and memory qualification. Buffer settings
do not cap certificates, handshake state, native contexts or allocator
overhead. Permit at most one pending application record per direction;
unread plaintext blocks intake until drained. Available handshake work must
advance without a new socket-read event. Prove progress with a
bounded-capacity transport, simultaneous writes in both directions, one-byte
fragmentation and short output buffers; unbounded in-memory pipes cannot
establish absence of a backpressure deadlock.

The backend may parse several handshake messages carried in one record, or
finish a message fragmented across records. The input/reassembly ceilings
bound that work; a call count alone supplies no elapsed-time bound on
certificate verification or native entropy. Deadline expiry is enforced
between calls by the owner. DESIGN.md's native blocking/abort limits remain
unchanged.

## Authentication evidence, failure and close

Progress distinguishes Handshaking, Open, Closing, Closed and Failed. A
handshake report exists only after full certificate verification and
Finished validation have succeeded, with no pending backend handshake. It
contains the negotiated TLS version and td-owned peer evidence:
Unauthenticated for ordinary inbound TLS, VerifiedServerName for outbound
TLS, or VerifiedClientLeaf with SHA-256 of the verified client leaf DER for
mandatory mutual TLS. It is bound to this session/configuration; it is not a
reusable authorization credential. Do not derive success from presence of
backend peer certificates alone.

The mail adapter combines VerifiedClientLeaf with current gateway pin and
socket-address policy before constructing its Gateway proof. It binds
outbound VerifiedServerName to the configured endpoint and refuses
credentials before that proof. Policy-generation/slot leases stay entirely
in td-mta. No raw peer certificate, claimed SNI, debug string or provider
handle crosses this API.

Fixed TLS errors have seven categories: Capacity, Invalid, KeyMismatch,
Verification, Protocol, Clock and Crypto. Verification carries a fixed
td-owned reason: Missing, Untrusted, Expired, NotYetValid, Name, Usage,
Signature or Other. These same reasons apply to local certificate admission:
expired/not-yet-valid dates, missing SAN coverage, CA-leaf/wrong-usage and
bad issuer signatures map to their corresponding reason. Malformed
encodings, unsupported algorithms, duplicate/misordered local chains and
malformed/empty configured trust bundles are Invalid. A well-formed
certificate/private-key disagreement is KeyMismatch. No error carries
backend diagnostics, paths, peer-controlled strings or secrets. The control
path may attach trusted profile/index context.

Blocked is progress, not an error. Every returned session error retires the
connection: discard pending plaintext/ciphertext, invalidate handshake evidence
and refuse later operations except status/abort/drop. Constructor errors return
no session. A plaintext write after local write closure is explicitly terminal
Invalid, including discarding any queued close_notify; the facade refuses it
before the backend can encrypt bytes after close. Repeating the close operation
itself is idempotent. Session/config Debug exposes no sensitive backend state.

Narrow unwind boundaries retire affected state. A failure confined to session
protocol state retires that session. A caught unwind or identified local failure
inside a shared signing-key operation also permanently retires that shared
key and configurations selecting it. All clones/sessions check that lifecycle
fence before use and again before publishing results from an in-flight key
operation; new calls refuse Crypto. Rebuild through full cold admission and
generation accounting. Immutable policy does not prevent a shared retirement
flag. A retired shared clock similarly refuses new session work; ordinary
missing time retires the affected session without retiring that source.
Ordinary remote verification/protocol refusals do not retire shared keys.
Native abort, OOM and panic hooks retain DESIGN.md's limits.

Requesting clean close stops new plaintext writes and appends close_notify
after queued output. Drain it completely before transport close completion;
completion does not prove receipt. Receiving close_notify allows draining
previously authenticated plaintext, then reports clean read EOF. In TLS 1.3,
the local write half may remain open. After peer close, intake consumes and
discards any further syntactically framed, size-bounded record without calling
the backend or reopening the read half. Malformed framing still fails Protocol.
This is explicit discard progress, not the backend's zero-byte read result.

TLS 1.2 requires prompt close_notify response and write closure. Discard
unencrypted pending writes. Ciphertext that consumed sequence numbers cannot
be omitted before another encrypted record: receiving a visible TLS 1.2 alert
record with pending encrypted output returns Protocol and aborts, whether that
output remains inside the facade or in a caller socket-write tail. Otherwise
process the alert normally and queue the close response when appropriate.
Never splice a close alert after omitted or partially transmitted ciphertext.
The version-specific rules are in [RFC 5246 §7.2.1](https://www.rfc-editor.org/rfc/rfc5246.html#section-7.2.1)
and [RFC 8446 §6.1](https://www.rfc-editor.org/rfc/rfc8446.html#section-6.1).

Transport EOF without received close_notify is terminal Protocol. During
handshake, transport EOF or backend-reported closure fails. Additionally,
refuse any visible alert record before handshake completion, including TLS 1.2
encrypted alerts: its outer content type remains visible. This deliberately
refuses pre-handshake warning alerts instead of inheriting the backend's
ignored TLS 1.2 close_notify warning. TLS 1.3 protected alerts are processed
by the backend; neither an error nor closure may produce handshake evidence.

Reads may continue after local close, but any subsequent error discards all
backend output, including a newly queued fatal alert; emit no record after
local close_notify. The pinned backend can hit a debug assertion on this path
in host builds while release queues an alert. Exercise malformed/oversized and
bad-MAC records after local close in both profiles: no unwind escapes, no
subsequent alert/application output is emitted, and the session is retired.
A caught session-state debug assertion maps to Crypto; ordinary protocol
refusal maps to Protocol. Neither implies a shared key failure.

Once both directions are closed and output is drained, report Closed. Abort
immediately fences operations, clears retained I/O as hygiene and emits no
records; the adapter also closes the socket. Close/abort are idempotent.

## Integration and qualification order

1. Implement bounded material codecs and admission with malformed, wrong-key,
   expiry, name, usage, chain and capacity fixtures. Keep loading unavailable
   to service until complete generation accounting is qualified.
2. Implement the explicit policy/configuration layer and prove exact version,
   suite, group and signature inventories, independent resumption refusal,
   private-root replacement, SNI and time-source behavior.
3. Implement the opaque socket-free session and progress/error/close semantics.
   Exercise fragmented records, short buffers, blocked writes, malicious
   handshake lengths, no premature evidence and terminal failure injection.
4. Integrate td-mta's TlsFactory/TlsTransport with leases, deadlines and explicit
   STARTTLS flush/tail refusal/reset; test implicit client TLS and gateway
   pin/IP admission through the public facade only.
5. Measure Rust/native allocations, stack and whole-process RSS for complete
   generations, all admitted sessions and worker warm-up/reseeds. Qualify
   normal and refused peers on the isolated musl artifact, then enable service.

No row authorizes new dependencies or unsafe surfaces. Every implementation
increment receives the normal review and ready workflow. Tests contact only
local peers; credentials, live smarthosts and public CAs are unnecessary.
