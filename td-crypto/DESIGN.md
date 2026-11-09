# Cryptography and TLS boundary

## Status and ownership

This crate owns td's provider-independent cryptographic API. The compiling
M03a surface contains `Error`, `Entropy`, `Digest` and `Crypto`, extracted
from td-mta's existing ports. M03b1 admits the private Rustls dependencies
and checks their offline host build. M07a1 implements opaque
streaming SHA-256; its direct operation uses the owned fixed-state
primitive. M07a3 adds the opaque worker-local entropy handle. M07a4
implements the Crypto factory, fixed-size comparison and opaque P-256 key
generation/loading/signing. M07b1 adds bounded certificate PEM syntax and a
P-256 PEM key loader. M07b2a adds cold admission of an owned local server
identity. M07b2b adds explicit private CA bundles and the pinned public
server root set. M07b3a supplies the explicit private TLS algorithm
provider. M07b3b retains owned keys for TLS signing; M07b3c1 adds the
shared clock and outbound ClientConfig. M07b3c2 adds ServerConfig and
immutable identity routing. M07c1 adds socket-free client sessions; M07c2
adds server admission and verified-client evidence.
Test-only backend qualification covers explicit provider construction,
SHA-256, local TLS 1.2/1.3 data exchange, certificate verification and
malformed/tampered-record refusals in the isolated static executable. The
backend is td-primitives, for TLS through td-tls-provider; fixed known
answers from AWS-LC, the backend it replaced, pin compatibility
("td-primitives backend qualification" below). Mocks and interface tests are
not cryptographic conformance evidence. In this document "native" names the
backend's own objects and calls (Rustls, td-tls-provider and td-primitives),
as distinct from the facade's.

The target dependency graph is:

```text
td-mta (library and packaging executable)
  -> td-civil (std-only civil time and timestamp parsing)
  -> td-header (std-only bounded header lexical cursors)
  -> td-html (std-only bounded HTML text extraction)
  -> td-json (std-only bounded JSON string framing)
  -> td-mime (std-only MIME/header/charset/Unicode implementation)
  -> td-nfc (std-only bounded canonical composition)
  -> tokio-postgres (private PostgreSQL backend, runtime disabled)
  -> td-crypto (td-owned API and private backend)
       -> rustls (no built-in backend) + reviewed root data
       -> td-tls-provider (rustls CryptoProvider over td-primitives)
       -> td-primitives (std-only primitives)
```

td-mta/DESIGN.md section 3 owns the direct mail dependency rule. After backend
admission, td-mta is this crate's only permitted roster consumer. The common
gate must reject other direct or transitive consumers; the engine workspace
and other std-only roster crates cannot inherit its external closure.

The mail lock admits the user-approved tokio-postgres 0.7.18 closure
with default features disabled. SCRAM authentication uses private hmac/sha2,
stringprep and rand; md-5 compiles in the upstream protocol crate, but mail's
connection policy refuses legacy database authentication. These implementations
stay inside the private mail driver and do not widen this crate's API or TLS
provider. Tokio supplies I/O traits and synchronization only; its runtime,
net and time features are absent. No other roster consumer is admitted.

The common offline vendor uses the mail lock's union of crypto and PostgreSQL
sources, checking every archive checksum. Crypto's selected graph is eight
registry packages, td-tls-provider's seven and webpki-roots; mail adds the 56
selected PostgreSQL-side packages listed in DEPENDENCIES.md. Exact manifests, locks and active features are pinned in the
shared policy. Mail's POSTGRES.md owns SQL and database resource semantics;
STORAGE.md owns local runtime and response scratch. Native mail qualification
requires independently installed PostgreSQL binaries; these host test tools
are not inputs to a target artifact.

`td-crypto` does not depend on td-mta, its identifiers, configuration grammar,
mail protocols, storage, logging, workers or service administration. Its own
code uses std and follows the panic/indexing and unsafe contracts. Its
backend is the approved Rustls engine over td-tls-provider and td-primitives.
AGENTS.md and the shared
host/sandbox gate admit only the exact named manifests, locks, root Cargo
configuration and selected normal/build features in
`builder/src/crypto_policy.rs`. Other roster closures remain std-only.
Changing any pinned input requires an explicit policy update. The
checkout's root `.cargo/config.toml` is required. Ancestor
`.cargo/config.toml` files are accepted only when each matches that same
compiled pin, allowing nested worktrees to use their own runner. Legacy
`.cargo/config` files remain refused at every level. Automatic build.rs
files are refused for all eleven local source packages: td-civil,
td-crypto, td-fs, td-header, td-html, td-json, td-mime, td-nfc, td-mta,
td-primitives and td-tls-provider. Their exact manifests and locks are
pinned; td-civil, td-fs, td-header, td-html, td-json, td-mime, td-nfc and
td-primitives remain std-only roster crates. td-primitives' dev-dependency
td-source-scan is compiled into no admitted graph and is not pinned.
td-crypto depends on td-tls-provider and td-primitives by path; td-crypto's
and td-mta's selected graphs contain both. td-tls-provider (its DESIGN.md) is
admitted with its own pinned Rustls line, which td-crypto's matches, and
active graph, which select no backend. The Cargo config pin contains only target runner settings, for
which Cargo selects the deepest definition. Any future pin change must
recheck ancestor merging and relative path behavior; identical files with
other settings need not have identical effective behavior. A nested
worktree that changes the pin therefore requires matching ancestor configs
too; an older worktree must rebase after an ancestor pin changes. Config
reads require regular files of at most 4096 bytes and are bounded even if
a file grows. These are trusted host checkout inputs, not a defense
against concurrent hostile path replacement. `DEPENDENCIES.md` records the
complete locked inventory and active subset.

The host preflight prepares checksum-pinned crate archives through td-feed
before compiling. The loop attempts preparation after provisioning its userland and before
entering its networkless sandbox, respecting disabled gates. Preparation there
is best-effort: a provisioned Cargo gate enforces presence, while a missing
toolchain retains the existing Unprovisioned result. The host preflight fails
if its required source preparation cannot complete. Both command lists use `td-builder gate-crates crypto-cargo` for
td-crypto, td-mta and td-tls-provider. This wrapper verifies private archive copies, reconstructs
and rehashes extracted sources before cache reuse, uses a fresh Cargo home,
forces offline/frozen source replacement, and checks Cargo's selected graph.
A missing, stale or tampered vendor fails; Cargo never fetches a substitute.
The native td-mta test leg additionally builds td-mail and a td-net test
helper for the real-client read fixture. These host-only helpers follow their
existing roster/locked-source policies, not the private crypto active-graph
allowlist. The td-net archives are prepared only for native mail tests;
portable crypto/mail builds and crypto-only qualification do not require them.
DEVELOPMENT.md owns helper selection and test-runner bounds; td-mta's
CONFORMANCE.md owns the fixture's evidence and limits.
The wrapper qualifies the provisioned host Rust toolchain, with the
provisioned `cc` (td-link on a td host) as its linker driver only: no
admitted crate compiles C, and the wrapper sets no C compiler or archiver
for build scripts. Host invocations
share `.td-build-cache/crypto-target` unless CARGO_TARGET_DIR is supplied.
Verified archive extraction and tree hashing repeat for each invocation; a
persisted digest alone cannot authenticate changed source bytes. Its build is
not the portable artifact: M03b2 separately pins all portable inputs and
proves the clean musl build (PORTABLE.md).

## Public API and failure rules

Public functions, traits, errors and opaque handles use only std types and
types defined by td-crypto. Never re-export upstream modules, type aliases,
provider configuration types or error chains; a public function returning a
Rustls type would breach the boundary even without a direct consumer dependency.
Concrete backend state lives in private fields/modules. There is one selected
implementation per operation in source, with no feature, environment or runtime
backend selector. The allocation-free `Sha256` and worker-oriented `BulkSha256`
are distinct resource contracts with fixed implementations, described below.

`Entropy::fill` returning success fills the entire caller slice. On a returned
error, discard the whole output, including bytes written before the error.
`Digest` streams SHA-256 and consumes its state to return exactly 32 bytes.
After any update error, callers must discard the operation; implementations
must also refuse every later update and finish. This includes test providers.
`Crypto` supplies fallible SHA-256 construction, fixed 32-byte constant-time comparison,
P-256 key generation/loading, public-point encoding and ES256 signing. Associated
key/digest types permit test implementations; shipped implementations must use
td-owned opaque types, not upstream types as their public associated types.

P-256 private keys cross the boundary as PKCS#8, public points as 65-byte
uncompressed SEC1, and ES256 signatures as 64-byte r || s. Key generation/load
are cold operations; callers supply output capacity and account for retained
key state. Signing consumes message bytes and applies SHA-256 once as part of
ES256. No undocumented prehash convention. `equal_digest` uses reviewed
constant-time code; ordinary Rust equality is not its production implementation.

After public-point extraction or signing fails, discard the key; implementations
must refuse later operations, including mocks. Crypto output buffers remain
unchanged on returned errors.

The four fixed errors are Capacity for insufficient output, Invalid for
malformed/unsupported/inconsistent key or certificate-envelope input, Entropy for recoverable random
source failure and Crypto for other recoverable provider operation failures.
They expose no provider diagnostics, nested provider error, secret bytes or
input-dependent formatted message. td-mta re-exports the shared traits and
maps these errors into its own port error enum. The shared crate never imports
that enum. The future TLS configuration, progress, error and authentication
contract is
[TLS.md](TLS.md). M07 implements its concrete interfaces before consumers;
they follow the same ownership and redaction rules.

## Backend and TLS implementation

### Worker-local entropy

`SystemEntropy::try_new` performs a nonempty one-byte random fill before
returning a handle. Construct it on its owning worker during cold setup. The
handle is neither Send nor Sync, so it cannot transfer an initialized handle
to an unwarmed thread. Empty fills succeed without reading. No Default
constructor bypasses warm-up.

Each nonempty fill is td-primitives' `random::fill`: it opens `/dev/random`
and reads exactly the requested bytes, so a process using td-crypto needs
`/dev/random` reachable and readable in its mount namespace; where it is
not, construction and every fill return Entropy. Since Linux 5.6 that read blocks only until the
kernel's CRNG is initialized, so the constructor's fill, and any fill before
that point, can wait without a deadline; this constructor supplies no timeout
or cancellation mechanism, and M07 must qualify startup ordering and
supervision around it. The handle holds no state and no Rust heap buffer;
there is no per-thread or process-wide generator state, no reseeding and no
exit-time cleanup. Each fill opens and closes one file descriptor, so
descriptor exhaustion is a returned failure. Constructor success establishes
no bound on later elapsed time.

An open or read failure, a short read among them, maps to fixed
`Error::Entropy` and clears the entire caller slice. Callers must still
discard it; zeroed bytes are never successful entropy. There is no
weak-randomness or partial-byte fallback. This is observable hygiene, not
guaranteed erasure of compiler/caller copies. Private callback tests inject a
returned error after a partial write at the same boundary. A local sample
comparison is only a no-op/constant-output wiring check, not an
entropy-quality test.

### Streaming SHA-256

`Sha256::try_new` creates one inline td-owned operation with eight chaining
words, one 64-byte partial block, a checked byte counter and terminal-state
flag. Construction, updates and consuming finish use no heap, provider calls,
locks or OS calls. The fallible public API remains unchanged. The concrete
`Crypto` factory delegates to this constructor. This direct streaming operation
uses the owned primitive; bulk worker hashing, TLS and ES256 signing use
td-primitives' SHA-256.

The implementation adapts the FIPS 180-4 constants and compression algorithm
already present in engine/src/sha256.rs. It owns its checked streaming and
padding logic inside this crate; no engine helper or allocating file/string
adapter enters the production dependency graph. Compression uses a fixed
64-word schedule, 64 rounds and wrapping 32-bit additions. Loop counts and
memory addresses depend on public message length/round number, not message
contents. No data-dependent lookup table or hardware dispatch is used.

Checked total input length cannot exceed `u64::MAX / 8` bytes. Padding writes
the original big-endian bit length directly, without passing padding through
the counted update path. Length refusal or an internal bounds failure clears
and retires the state; later updates and finish return `Error::Crypto`. Only a
successful consuming finish returns 32 bytes. No clone/reset or alternate
backend selector is exposed. This implementation has no provider panic to
catch; the crate still requires unwinding for its other operations.

Retirement and Drop clear retained words, block and counters; compression
clears its schedule. `std::hint::black_box` follows those writes to discourage
dead-store removal in the qualified build. This is observable hygiene with
an optimization barrier, not a portable secure-erasure guarantee. Rust moves,
registers, compiler temporaries, caller input and abort/kill paths can retain
copies. No unsafe volatile-write helper or new dependency is introduced.
In the inspected x86-64 musl build, update refusal and drop without finish
clear state in place. Consuming finish clears a moved copy; the caller's
original stack slot and compression spills can retain message/state bytes.

Tests retain the four existing known answers and compare boundary lengths,
fragmentation and every split through 257 bytes against td-primitives'
separately written, CPU-dispatched SHA-256. Engine-derived code is not an
independent oracle for this implementation. Synthetic counters exercise the
limit without hashing exabytes;
counter differences also check that each upper padding-length byte matters.
Malformed private-state fixtures check fail-closed retirement. The portable
harness runs these cases and the existing mail-format digest fixtures. Its
inline state is at most 128 bytes; source and exact-artifact call-graph review
qualify the no-allocation and content-independent control/address claims for
the recorded compiler/flags/target. Repeat artifact review when those change.
This does not complete whole-service stack/RSS qualification or qualify other
primitives, targets or microarchitectural leakage outside that review scope.

### Bulk worker SHA-256

`BulkSha256` is an opaque streaming digest for bulk I/O workers, implemented
with td-primitives' inline SHA-256 context, which uses the CPU's SHA
extensions where a runtime check finds them. It implements `Digest` but does
not replace `Sha256` or the `Crypto::sha256` factory. Its contract permits
allocation and CPU dispatch; it makes no allocation-free, constant stack/RSS,
or portable secure-erasure claim. The caller bounds simultaneous contexts,
input chunks and total input, and checks deadlines between chunks. No backend
context, algorithm choice, clone or reset crosses the API.

Construction and consuming finish catch Rust unwinds and return only Crypto.
Each update removes the context before checking the cumulative
`u64::MAX / 8` byte limit and before entering the unwind boundary. Only success
restores it. Length refusal, returned error or unwind permanently retires the
operation, including after a partial update. A retired context is dropped
without clearing; compiler copies and abort/kill paths retain the ordinary
limitations. Abort, OOM and panic hooks are not contained. Tests compare
padding and chunk boundaries with the independently qualified owned digest
and inject update errors/unwinds; no test asserts a wall-clock performance
threshold.

### Crypto factory and P-256 signing keys

`Provider` is a stateless td-owned implementation of `Crypto`; creating it does
not initialize state or warm a worker. Its SHA-256 factory delegates to
`Sha256::try_new`. Fixed 32-byte equality calls td-primitives' `ct::eq`,
which folds the XOR of every byte pair into one accumulator without an early
exit and passes the result through `std::hint::black_box`, with no
ordinary-equality fallback. That
is a source-level property; td-primitives/DESIGN.md records that no
instruction-level inspection has confirmed it in a compiled artifact, and
functional comparison tests do not establish exact-artifact timing behavior.

`P256Key` owns a td-primitives P-256 secret key and the public point derived
from it once at load, behind a private mutex. Public-point extraction and
signing serialize on that handle. Each operation borrows the key where it
lies, inside the same narrow unwind boundary, so a signature moves no copy
of the scalar. A returned
operation error or Rust unwind drops the key and leaves the handle retired.
Later operations fail Crypto; a poisoned mutex also refuses access. No clone,
private-scalar export or backend handle is exposed. The mutex can block;
these operations have no timeout/cancellation mechanism. M07 must fit actual
usage into the control-worker lifecycle before enabling service.

Key input is one complete unencrypted PKCS#8 v1 DER PrivateKeyInfo (version
integer zero), capped at `P256_PKCS8_CAPACITY = 150` bytes. The outer algorithm
must contain exactly id-ecPublicKey and named-curve prime256v1 OIDs. Its private
OCTET STRING contains one SEC1 ECPrivateKey sequence: version one, exactly
32 scalar octets, optional explicit [0] containing the same curve OID, then
optional explicit [1] containing a zero-padding BIT STRING with a 65-byte
uncompressed point. All lengths use minimal definite DER encoding; every
container must end exactly. The four accepted shapes are 67, 79, 138 and 150
bytes, depending on the two optional inner fields. That parse is structural.
The loader then refuses a scalar outside 1..n-1 and an embedded point that
differs from the one it derives, both Invalid, deriving the public point when
omitted. Structural acceptance alone is no key proof.

The DER entry point refuses encrypted keys, PEM, bare SEC1, PKCS#8 v2,
attributes, explicit curve parameters, compressed/hybrid/infinity encodings, overlong/indefinite lengths,
wrong/duplicate/reordered fields and trailing bytes at every level. These are
intentional subset restrictions. The formats follow
[PKCS#8](https://www.rfc-editor.org/rfc/rfc5208) and the
[ECPrivateKey structure](https://www.rfc-editor.org/rfc/rfc5915); curve
parameters may be inherited from the outer algorithm, the conventional
PKCS#8 encoding.

Generation requires caller capacity of at least 150 bytes before drawing.
It draws 32 bytes from td-primitives' `random::fill` and takes them as the
scalar when they lie in 1..n-1, drawing again otherwise (about once in four
billion draws), at most 64 times: a random failure maps to Entropy and
exhaustion to Crypto. It writes the 138-byte document with an inner public
point and curve parameters only in the outer algorithm, the shape AWS-LC
generated, and validates it against the same accepted subset before copying.
Return the used length and leave the unused tail unchanged. Public-point
output is exactly 65-byte SEC1; ES256 output is exactly 64-byte r || s.
Signing hashes the complete message with SHA-256 once; it accepts no
prehashed convention. Refuse messages beyond the SHA-256 bit-length limit.
Output buffers remain unchanged on returned errors; callers must treat them
as having no result. Caller-owned private-key input and generated output
remain the caller's secret-lifecycle responsibility.

Signing is td-primitives' ECDSA with RFC 6979's deterministic nonce from
HMAC-SHA256, so equal messages under one key give equal signatures and a
failed random source cannot repeat a nonce; it uses no entropy. It returns a
DER signature in a heap vector, which td-crypto converts to r || s, so each
signature allocates. This is not qualified for a no-allocation hot path.
A synthetic Rust error or unwind injection is not a random-failure test.

td-primitives' secret key clears its scalar words when dropped, and its
signer clears the nonce and the intermediate values it names, best effort;
td-crypto clears its scalar draw and its local copy of a generated document
with `ct::zero`. td-primitives/DESIGN.md's erasure non-claims apply: the
RFC 6979 generator's HMAC state, registers, stack spills, moved values,
caller buffers and the DER signature vector are not cleared, and abort/kill
bypasses every cleanup. Ordinary parse failures and an out-of-range or
inconsistent key map to Invalid; generation entropy failure maps to Entropy;
other generation/signing failures and caught Rust unwinds map to Crypto. No
panic payload becomes a public error. The whole-graph unwind and hook
limitations below apply.

This boundary requires Rust unwinding throughout the target graph, including
the final executable. The crate refuses its own compilation with `panic=abort`;
that check alone cannot detect linking an unwind-built rlib into a separately
compiled aborting executable. The supported portable Cargo build compiles the
whole graph with the default unwind strategy. Every future target recipe or
consumer build must preserve that strategy through final linking; a final-only
abort override is unsupported. Panic hooks still execute, and an aborting or
panicking hook is outside the boundary. Aborts, OOM and unwinding failures are not contained.
P-256 failure tests inject Rust unwinds at its private operation boundaries;
they do not establish allocation-failure or entropy-failure recovery.

Tests use only existing repository crypto material: the engine SHA-256 and
td-fido P-256 source/vector fixtures are compiled as test-only independent
oracles for the ES256 signer. Their own known-answer, valid/invalid
signature and arithmetic cases
qualify the oracle; generated signatures must verify against the message,
not its digest, and modified messages/signatures are refused. These sources
never enter the shipped backend or add a Cargo dependency. Portable staging
includes the exact oracle source files and vector data in its source digest.
The persisted-key fixtures under "td-primitives backend qualification" below
pin compatibility with keys AWS-LC wrote. Functional, grammar and failure
qualification does not complete allocation, timing, stack/RSS or service
integration work.

### Bounded PEM material syntax

`PemCertificates::chain` and `trust_bundle` borrow a complete bounded input
and validate every envelope before returning a reader. The reader stores
only the remaining slice and count. `decode_next` writes one certificate to
a caller buffer without allocation; insufficient capacity leaves the reader
and output unchanged for retry. Success leaves the unused output tail alone;
exhaustion returns None without writing. `CERTIFICATE_DER_CAPACITY` gives
callers a sufficient buffer size. Debug reports only the count.
The parser is a cold-path syntax interface, not verified identity evidence.

TLS.md owns the byte/count limits. A constructor refuses empty input, wrong
or mixed labels, headers, unknown text, invalid padding, and a malformed,
nonminimal or incomplete outer DER SEQUENCE. Inner certificate fields,
duplicate certificates, dates, names, signatures and trust remain M07b2.
The constructor publishes no partial reader when a later block is invalid.
The text cap is checked before scanning; fixed counters and a four-byte DER
prefix replace input-sized temporary storage. The complete borrowed input
cannot change while a reader exists.

BEGIN may follow whitespace between blocks, including on the same line.
END starts its own line; neither marker permits trailing whitespace. LF and
CRLF are accepted, including mixed line endings; a final footer need not
have a newline. All six ASCII whitespace bytes are permitted between blocks.
Inside
base64, permit space, tab, LF and CRLF only. Standard alphabet and canonical
padding are required; padding is present exactly when the final quantum
needs it. No data may follow a padded quantum within a block. An empty DER
SEQUENCE is refused. These are intentional PEM subset restrictions.

`Provider::load_p256_pem` accepts exactly one PRIVATE KEY block, decodes
into a fixed 150-byte temporary, and delegates to the existing strict DER
and provider key checks. It does not match a key to a certificate. Its
borrowed input cap is 16 KiB; malformed/over-limit material returns Invalid,
while caller certificate output shortage returns Capacity. The temporary
clears on normal return, returned error and Rust unwind, with black_box after
clearing. That supplies ordinary hygiene only: decoded quantum temporaries,
registers, caller input and compiler copies may remain, and abort/kill can
bypass cleanup. Key construction retains the failure limits above.

Tests compare certificate decoding with Rustls pki-types' PEM reader over padding, byte-value and DER-length boundaries; local generated
keys compare decoded PKCS#8 and public points. Exact input/count/DER limits,
truncations, malformed envelopes/padding, retry and output preservation run
in host tests and the isolated portable artifact. Syntax acceptance does
not validate X.509 or complete the TLS allocation/stack/RSS qualification.

### Local server identity admission

`ServerIdentity::from_pem` accepts a bounded leaf-first chain, one P-256
PKCS#8 PEM key, one to 32 exact DNS bindings, and optional UTC seconds.
It publishes one owned identity only after all checks pass. It retains no
caller borrow and performs no filesystem, socket, DNS, trust-download or
system-clock operation. Names follow the mail configuration's ASCII hostname
shape, are folded to lowercase, and reject wildcards, IP literals, trailing
dots, underscore labels and duplicate folded bindings. SAN matching uses
the backend verifier, including ordinary certificate wildcard semantics.

The identity retains its certificate bytes, names, opaque P256Key and the
inclusive validity intersection of every supplied certificate. Inspection
methods return only public certificate/name data and counts; Debug shows
counts only. Out-of-range inspection returns None. `check_validity` checks
supplied time and key health again. Configurations must still recheck at the
selection/completion points specified in TLS.md. This handle has no TLS
session or mail-authorization operation, and is not a remote trust grant.

`restrict_names` cold-constructs an identity with a nonempty subset of already
admitted exact bindings. It applies the same canonicalization/duplicate limits,
refuses widening even to another name present in the certificate SAN, and
shares the existing key/certificate-chain owners. It preserves validity bounds
and checks key health; retirement remains shared. It performs no new key import,
certificate parsing, trust operation or time extension. The new name vector and
small identity/shared-owner headers belong to the caller's existing generation
budget. TLS configuration/session time and health fences still apply.

The owned metadata reader precedes backend parsing. It uses checked slice
access, minimal definite lengths, fixed depth, a four-digit calendar and at
most 64 extensions per certificate. Duplicate extension OIDs, malformed OID
encodings, noncanonical known BOOLEAN/named-bit encodings and incomplete
containers are refused. Every SAN GeneralName envelope is checked, including
entries after a matching DNS name; IA5 forms must be nonempty ASCII, IP forms
must have four or sixteen octets, and registered IDs must be canonical OIDs.
Structured non-DNS GeneralName contents remain opaque; this is not full
semantic validation of every X.509 name form.
X.509 v3 is required, matching the backend parser;
issuer/subject names and serial compatibility otherwise follow its supported
X.509 parsing. Dates use whole-second Zulu UTCTime or GeneralizedTime,
1970 through 9999, with ordinary Gregorian leap years and no leap seconds
or fractions. Pre-1970 dates are unsupported by the pinned backend. Every
supplied certificate, including a supplied root, must be current.

Require a non-CA P-256 leaf with the exact uncompressed public point of the
loaded key. Issuers must be CAs. Present KeyUsage must permit digitalSignature
for the leaf or keyCertSign for an issuer; a non-CA leaf cannot claim
keyCertSign. Present EKU must include serverAuth; anyExtendedKeyUsage alone
does not satisfy the pinned backend. Absent usage extensions are allowed.
RSA issuer modulus size is 2048 through 8192 bits; the provider validates
actual key/signature mathematics. Remaining supported issuer algorithms
follow TLS.md's classical certificate inventory.

The certificate algorithm table is td-crypto's own private static: nineteen
td-tls-provider verifiers in the order of the exact public-key and signature
AlgorithmIdentifier pairs it pins, including parameters. The selector checks
the count and every pair before returning the slice; drift refuses with
Crypto. It names no ML-DSA algorithm, and no global provider is consulted.
The same private provider also validates handshake mappings.

Reject duplicate or misordered certificates, and repeated issuer
subject/key pairs even when their certificate bytes differ. The key
comparison uses validated, reserialized SubjectPublicKeyInfo, so
compressed/uncompressed EC point aliases compare equal. Original
certificate bytes remain unchanged. This prevents backend path building
from bypassing an issuer and its constraints through an equivalent anchor
or intermediate. Check every supplied adjacent issuer name and signature,
and the last certificate's self-signature when self-issued. Enforce issuer
path-length limits, excluding self-issued intermediates in the owned
check. The backend additionally counts self-issued intermediates against
non-anchor path-length limits, so some rollover paths that pass the owned
check are refused. Its stricter refusal can report Other or Signature
rather than Usage. For a chain with supplied issuers, also run backend
path/name constraint verification using the last supplied issuer as a
temporary consistency anchor. Its dates, CA/usage and path length are
still checked locally; using it for consistency does not trust it on any
network peer.

An omitted issuer is allowed at a non-self-issued chain tail. Its absent
key means the tail's issuer signature cannot be verified locally; that
includes a lone CA-issued leaf. A self-issued tail must also be self-signed
in this subset: a rollover certificate signed by a different same-name
issuer must include that issuer. Issuer/subject equality uses the backend's
exact encoded-name comparison. A provided self-signed leaf must verify its
own signature.
These checks establish local key/chain/name consistency, not a complete
public or private trust path. Peer verification and explicit/public trust
configuration remain separate. No fallback certificate or old identity is
published on refusal.

`TlsError` and `VerificationFailure` implement TLS.md's fixed categories.
Construction catches Rust unwinds, drops unpublished state and returns
Crypto. Input/format errors return Invalid; a mismatched valid local key
returns KeyMismatch. Missing time returns Clock. Time, name, usage and
signature failures carry the matching fixed verification reason; remaining
backend path constraints use Other. No provider error or input becomes an
error source. Abort/OOM and panic hooks retain the limits above. No secret
erasure, timing or whole-service resource guarantee follows from a returned
Result or a synthetic unwind fixture.

Owned vectors reserve checked bounded capacities; webpki path verification
and the key still allocate. M07e must charge inputs, decode scratch,
certificate/name copies, canonical public-key copies, path state, retained
keys and overlapping generations before the service can use this handle.
No listener or outbound adapter is enabled by M07b2a.

### Owned-key TLS signing bridge

Local identity admission retains its chain in one private shared CertifiedKey
and its existing opaque P256Key behind an Arc. The private TLS adapter retains
that same key, with no private-key export, reload or independent failure state.
Certificate byte vectors move into the retained chain without copying their
contents. Public inspection still returns the same certificate bytes and
counts. A cached 91-byte P-256 SPKI contains only public key material;
key/leaf matching checks it before publication. No backend handle crosses the
public API, and no reference cycle retains an identity.
ServerIdentity retains Send, Sync, UnwindSafe and RefUnwindSafe. The erased
backend signing trait does not express the last two; the owned identity
implements them because its sole adapter has immutable public state and the
existing mutex-fenced, retiring key. Tests pin the concrete adapter's inferred
unwind traits. This is not a promise that aborts or panic hooks recover.

The adapter advertises only ecdsa_secp256r1_sha256 and passes the unhashed
handshake message into the existing SHA-256 ECDSA operation. It converts the
64-byte r || s result into canonical positive DER INTEGERs and a SEQUENCE of
at most 72 bytes. Leading zero octets are removed, sign padding is inserted
when needed, and a zero scalar is refused as an internal failure. The
signer supplies valid scalars; this private encoder is not a separate public
signature-validation interface.

Signature construction and its fallible Vec reserve run inside the same
locked operation/unwind boundary that owns the key. A transform error
or unwind drops the key before restoration; all adapters/configurations
sharing it refuse later signing. The raw ES256 API uses that same boundary
with an identity transform, preserving its fixed output and error semantics.
A completed operation cannot retract signature bytes already returned; future
sessions must still recheck shared-key health at the TLS.md admission and
handshake-evidence publication points. Cached public bytes and certificate
inspection are not proof of live key state. Ordinary remote protocol/name
refusals do not retire the shared key.

Only fixed td-owned TlsError tags travel through the private Rustls error
channel. Public errors never include backend diagnostics, certificates,
signatures, names or secrets. Debug for the adapter/signer is fixed redacted
text. Rustls selector boxing, td-primitives' DER signature and the returned
signature Vec are TLS allocations; chain/header vectors, SPKI and Arc
ownership are cold generation costs. M07e still must measure them with
signing costs. Abort/OOM, panic hooks, blocking and secret-erasure limits
remain unchanged.

Tests cover canonical DER integer boundaries, verification under td-fido's
independent P-256 verifier and hash-once behavior, wrong scheme refusal, synthetic post-sign transform errors
and unwinds, shared retired-key refusal and unchanged raw ES256 outputs. Local
TLS 1.2/1.3 fixtures complete full handshakes with the owned signer, retain key
health after remote name/protocol failures, including receipt of the refusing
client's fatal alert after server signing, and refuse both configurations after
shared-key retirement. Resumption is disabled in those fixtures so it cannot
bypass the signing operation. These tests do not implement public TLS
configuration roles, clocks, sessions or gateway authorization.

### Trust-store admission

`TrustStore::from_pem` constructs an owned explicit private CA store;
`public_roots` selects only the already pinned webpki-roots server roots.
There is no append, merge, OS-store read, network fetch or fallback operation.
An explicit bundle replaces public trust completely. The immutable handle
retains its source marker and exposes only its count and whether the source
is public; Debug prints those two fields. It retains no caller input.

Explicit input uses the bounded trust PEM reader: nonempty, at most 128 KiB,
128 certificates and 16 KiB DER per certificate. Every certificate passes the
owned metadata parser and strict backend certificate parser; no best-effort
loader silently skips entries. A malformed later entry, duplicate subject/key
anchor or construction error refuses the whole store. Duplicate detection
uses the same canonical key representation as local identity admission. The local v3/date
syntax/extension-count subset above applies. Input time is not requested:
anchor certificate dates and signatures are not peer-path validation inputs.
Even an expired anchor or damaged self-signature can supply explicitly
trusted name/key material. This is trusted configuration, not a certificate
identity or proof that its issuer signed it.

The initial explicit private bundle is CA-only, with keyCertSign required
when KeyUsage is present. It does not implement leaf-certificate pinning.
Reject any anchor with an EKU, path-length or name-constraints extension in
this initial subset. Root conversion discards EKU/path length, while name
constraints need their own admission validation; none may silently become
broader trust. Such constraints can still occur on peer intermediates and
are handled by peer path verification. Supporting constrained private anchors
requires a separate admission/enforcement increment. Unknown critical
extensions are refused; other noncritical extensions retain the backend's
ordinary ignore semantics. A local self-signed server leaf remains a valid
ServerIdentity, independently of the remote endpoint's trust configuration.

Private anchor keys follow the classical public-key inventory: P-256,
P-384, P-521, Ed25519 or RSA 2048..8192. After exact raw-format checks,
construction validates the key with td-primitives. EC keys require
SEC1 points: uncompressed 04 with 65/97/133 bytes, or compressed 02/03 with
33/49/67 bytes for P-256/P-384/P-521, which td-primitives' `uncompressed`
checks are on the curve and stores uncompressed; the hybrid form and the
point at infinity are refused. Ed25519 takes any 32 bytes, as a verifier
would. RSA uses the minimal PKCS#1 public encoding already checked above,
which td-primitives' RSA public-key parser must accept: an odd modulus of
2048 through 8192 bits and an odd exponent of at least 3 and at most 33
bits. These are the keys AWS-LC admitted when it was the backend. Nested SPKI,
trailing bytes and incompatible encodings are Invalid. Local identity
issuers share these checks; the local P-256 leaf remains uncompressed only.
Key validation is not a peer signature proof. The certificate's unused
self/issuer signature need not use an admitted path-signature algorithm.
Peer verification still uses the separate pinned path/handshake inventories.

Public construction copies the vector of the compiled upstream anchors;
its subject/key/name-constraint bytes remain borrowed static data. The
reviewed set currently contains 118 anchors. Existing checksum/lock admission
pins the data; a test checks the complete retained list and count. It uses
upstream anchor constraints, without reparsing public roots through the
narrow private-CA certificate subset. Public trust is allowed only for
outbound server verification. M07b3's mandatory client-auth factory must
refuse a handle marked public; this constructor alone authenticates no peer.

Configured-bundle syntax, constraint, duplicate, key and CA/usage refusals
all return Invalid. Allocation-reserve failures return Capacity;
reserialization failures and caught Rust unwinds return Crypto. No peer
Verification result is produced by trust construction.

Cold construction catches Rust unwinds and drops unpublished state. Checked
vector reserves, decode scratch, public-key validation and owned anchor
copies remain part of the future M07e generation allocation qualification.
Abort/OOM/blocking and unwind-hook limits remain unchanged. Tests use
disjoint generated CAs to prove complete replacement, ownership after input
reuse, atomic refusal, caps, supported key families, constraints and fixed
public inventory. A private client-auth verifier fixture proves certificate
path usage only, not TLS Finished or gateway authorization. All cases also
run in the portable artifact; no listener is enabled by this increment.

### Explicit TLS algorithm provider

One private constructor supplies TLS.md's exact nine cipher suites, three
classical key-exchange groups, nineteen certificate algorithms and ten
handshake signature mappings. It starts from td-tls-provider's `provider()`
and selects its TLS 1.3 and ECDHE-RSA suites and its three groups by name, in
TLS.md's order, and that crate's `tls12_ecdsa` suites; a missing suite or
group refuses with Crypto. The provider's entropy and key provider come with
it. The certificate algorithms and scheme mappings are td-crypto's own
private table over td-tls-provider's `verifiers`, never re-exported. The
handshake selector checks the ten mappings, each scheme and every mapped
algorithm identifier pair in order. This pins the first ECDSA verifier used by
TLS 1.3 as well as the alternatives available in TLS 1.2. These checks pin
identifiers and ordering, not verifier behavior such as RSA modulus bounds;
the reviewed source/lock pins and the known-answer tests below retain that
behavior. Any signature identifier inventory drift returns Crypto; no global
provider is installed or consulted. The provider offers no ML-DSA verifier and
no hybrid group, so none can be inherited.

Local certificate admission uses this shared provider's certificate list.
The local backend TLS fixtures use it for both endpoints. Inventory fixtures
test truncated, expanded, reordered and substituted certificate tables and
mappings. An ML-DSA-44 root and the P-256 leaf it signed, written by AWS-LC
(whose verifiers accepted the path) when it was the backend, fail the
policy's certificate verification. Local handshakes exercise all three
classical groups under TLS 1.2 and 1.3, each TLS 1.3/ECDSA TLS 1.2 cipher,
refusal in both directions of a peer offering only a group named
X25519MLKEM768, and refusal by a policy client of a server whose only key
signs ML-DSA-44. Those peers are test fakes: no implementation of either
algorithm runs in these tests. The ECDHE-RSA suites and the RSA, P-384,
P-521 and Ed25519 handshake schemes are exercised against an independent
signing peer ("td-primitives backend qualification" below).

Construction catches Rust unwinds and publishes no partial provider. Cold
suite and group vectors allocate; the entropy, key, abort/OOM and hook limits
are unchanged. This is an algorithm policy component, not a configuration
handle, key-signing bridge or session API. Version selection, role policy,
SNI, ALPN, clock, resumption and resource qualification still require their
following increments before service use. td-tls-provider's key provider is
not restricted by these algorithm lists; it loads only one P-256 PKCS#8
shape. Production configurations use the owned P-256 signing bridge and
never load keys through with_single_cert or equivalent APIs.

### Outbound configuration and shared clock

`ClientConfig::new` constructs one immutable outbound configuration from an
admitted TrustStore, shared ClockHandle and TlsProtocol (Http1, Smtp or
client-only Postgres). Its
public API contains no backend types. Construction checks time once, copies
exactly the supplied anchors into the private verifier, and publishes no
configuration on failure. An explicit private store completely replaces the
public roots. There is no client identity, OS trust read, system-clock read,
network operation or provider default. Client configuration construction
alone authenticates no peer and exposes no session operations.

The configuration uses the explicit algorithm provider, TLS 1.3 followed by
1.2, mandatory TLS 1.2 EMS, outbound SNI and selected-ALPN checking. Http1
supplies only http/1.1; Smtp and Postgres supply no ALPN. Postgres follows
the classic SSLRequest upgrade, not direct TLS. Client resumption, early data,
key logging, secret extraction, ticket requests and both certificate
compression lists/cache are explicitly disabled. The non-ECH builder path
is used. The pinned fragmenter subtracts five header bytes from its explicit
size setting: 16389 admits one 16384-byte plaintext fragment. A local fixture
checks that one full application chunk emits one record in both versions.
These configuration choices do not bound every retained/temporary allocation.

ClockHandle owns one boxed UtcClock source behind a mutex. Its public now
method and the private backend time adapter use the same serialized callback.
The source must perform bounded, nonblocking work and must not reenter its
handle. No backend trait is implemented on a public td type. The handle's
Debug and the backend adapter's Debug reveal no source state or time. Native
UnixTime represents every u64 second value accepted by this interface.

Each call removes the owned source from its slot, invokes it inside a narrow
unwind boundary, and restores it only after a normal return. A None return
maps to Clock and keeps the source usable for another operation. A caught
callback unwind drops the source before releasing the mutex and permanently
retires this handle for every configuration sharing it; later callbacks are
refused with Crypto. Poisoned mutex access also permanently refuses. The
AssertUnwindSafe applies only to the consumed source, whose state is never
reused after unwind, not to a retained backend session. This cannot revoke
external aliases independently kept by a source implementation. Source
and panic-payload destructor failures, panic-hook failures, native abort/OOM
and blocking remain outside recovery. Constructor allocation and mutex
contention belong in qualification.

The backend time trait can express only missing time, so its private adapter
returns None for either ordinary unavailability or a retired source. The
session facade retains a private per-connection observation of
every callback failure and inspects it after every backend call, even a
successful call. It must also check shared retirement without another
callback before publishing results, and check time on operations for which
the backend requests none. Re-polling alone misses a transient None followed
by success. Failed sessions never revive when their clock recovers.

TLS 1.2 backend session saving requests time after server Finished when the
peer issued a session ID, even with client resumption disabled. It ignores
None and can report successful handshake completion after a clock failure
or caught callback panic. The new fixture pins both cases and demonstrates
why a later successful clock poll is insufficient. On normal time, that path
copies the master secret and peer chain before its no-op store drops them;
M07e must charge those temporary allocations and secret lifetimes. Other
fixtures cover missing time before construction, during TLS 1.2/1.3
verification and after TLS 1.3 Finished when a remote server sends tickets.
These backend observations are not public session success or failure evidence.

The private verifier checks at most eight presented certificates, at most
16 KiB DER each and 64 KiB aggregate before path/signature verification.
Refusal uses a fixed private Capacity tag. Every successful verification and
handshake-signature check delegates to the explicit classical WebPKI verifier;
there is no permissive result or skipped name/date/usage check. This callback
runs after backend message parsing. It does not prevent certificate-vector
allocation during parsing; M07c must enforce record/deframer intake ceilings,
and M07e must measure many small certificate entries and all parser overhead.

Tests pin configuration flags and local full handshakes, shared time-source
failure, disjoint trust replacement, wrong names, absent/known ALPN, server
refusal of non-overlapping ALPN and certificate-boundary enforcement. The
client's unoffered-ALPN check has inventory coverage only. Repeated connections
to a resumption-enabled remote server remain full handshakes in both versions.
These outbound configuration fixtures alone supply no server routing,
mandatory client authentication, public session, mail adapter or listener.
Those layers
must enforce their remaining TLS.md contracts before service admission.

### Inbound configuration and identity routing

ServerConfig constructs one immutable backend policy shared by up to sixteen
admitted ServerIdentity handles. It retains those same identities and their
owned signing keys; no private key is exported or reparsed. The native
certificate resolver and the td configuration share one immutable routing
object. It never references its parent configuration, so there is no cycle.
One policy avoids duplicating cipher/trust/verifier state for each identity.

TlsProtocol and IdentitySelection admit three combinations: Http1 with
RequiredName, Smtp with DefaultIdentity, and Smtp with MatchPresentName.
HTTP requires matching SNI. Both SMTP policies require exactly one identity;
DefaultIdentity accepts absent or unknown well-formed SNI, whereas
MatchPresentName accepts absent SNI but refuses present unknown names. These
are TLS selection policies, not mail roles or authorization. The constructor
refuses every other combination, empty/over-limit identity lists and duplicate
bindings across identities. At most 512 exact names are retained in the already
admitted identities; the routing table copies only their Arc handles. Pure
lookup scans at most 512 names with ASCII-insensitive comparison and allocates
nothing. The constructor rechecks all identities' time and key health once.
A cold candidate containing any invalid material is refused atomically. TLS.md
distinguishes that publication rule from continuing to serve other valid
identities in an already admitted configuration.

The resolver repeats the same pure lookup used by the session facade's
typed preselection. It does not call the clock or grant ongoing validity.
M07c inspects the initial ClientHello, selects an identity, checks that
identity's dates/key against the shared clock before backend signing, retains
that selection and rechecks it before publishing Finished evidence. An unrelated
expired identity does not prevent a valid identity from serving. The native
configuration alone does not enforce those session boundaries.

The pinned backend intentionally treats an IP literal in raw SNI as absent.
Acceptor::client_hello therefore loses information needed by the facade's
malformed-SNI refusal policy. A fixture replaces the native client's DNS bytes
with an equal-length IP literal and proves that loss. This is a backend hazard,
not admissible facade behavior. The same accepted hello selects the default
identity in both SMTP configurations and is refused by the HTTP configuration.
It does not complete a handshake: the mutation changes the transcript. M07c
performs bounded raw ClientHello/SNI validation before that information is
discarded, including fragmented and retry hellos. The pure routing helper rejects malformed names it receives,
but cannot reject a raw name already converted to absence. No public session
or listener bypasses this requirement in the configuration increment.

Client authentication is disabled with None or mandatory with Some private
TrustStore. Public-root-marked stores are refused. The verifier uses exactly
the admitted anchors and the explicit classical provider, enforces the same
eight-certificate/16 KiB each/64 KiB aggregate limits before path work, and
checks client-auth usage, time and handshake signatures. It has no optional
or permissive verification mode. As for the outbound verifier, native message
parsing precedes that callback and must be included in session memory bounds.

CertificateRequest carries no CA-subject hints. Clear the backend hint list
explicitly: a maximum-size configured bundle can otherwise yield an oversized
16-bit hint vector. An empty list permits the peer to choose a certificate;
mandatory path verification still applies every configured trust restriction.
This avoids transmitting a list too large for the handshake length field.
The initial bounded builder copy still belongs in M07e accounting. No gateway
leaf pin or socket-address authorization is performed by this crate.

Set the explicit TLS versions/provider, server cipher-order preference, TLS
1.2 EMS and 16384-byte plaintext fragment limit. Disable session storage,
stateless tickets, initial and requested TLS 1.3 tickets, early and half-RTT
data, key logging, secret extraction, certificate compression lists/cache.
Http1 selects only http/1.1, accepting absence; Smtp selects no ALPN.
ServerConfig refuses the client-only Postgres selector. Native
configuration/verifier/ticket/routing types remain private and their Debug
output discloses no keys, certificate bytes, names or callback state.

Tests cover cold roles/counts/duplicate bindings, 512-name routing, selection
between distinct owned keys, missing/unknown SNI, HTTP ALPN absence/selection,
full repeated TLS 1.2/1.3 handshakes against a resumption-enabled client,
mandatory-client proof and typed certificate/count/size refusals, supplied
verification time and the raw-IP SNI hazard. Backend connections in these
fixtures do not substitute for public session enforcement. Constructors and
provider work allocate within the still-unqualified TLS allowance; no service
is enabled until M07c/M07d/M07e complete their remaining contracts.

### Socket-free client sessions

`TlsSession::client` retains one ClientConfig and one owned, checked DNS name
inside a private backend connection. M07c1 supplies the client role; M07c2
adds the server role described below. No socket, DNS,
mail policy or listener is created. The caller assembles one complete TLS
record in its bounded wire buffer, retains socket-write tails, and aborts its
transport on every session error.

Public progress uses fixed td-owned TlsVersion, TlsPhase, TlsStatus,
HandshakeInfo, PeerEvidence, BlockedOn and TlsProgress values. Status is the
cached result of the last completed operation, with phase, separate read/write
closure, drainable byte counts, input/write readiness, optional handshake and
fixed failure. Inspecting it runs no backend work or clock callback; it is not
a fresh authorization or clock-health query. A shared-clock fault is observed
by the next operational call. Handshake evidence is published only after full
backend Finished success, fixed ALPN checks and the operation's clock fences.
Only VerifiedServerName is currently exposed, bound to the constructor's DNS
name and configuration. Copies never become independent authorization tokens.

Each connection gets a private native configuration clone with one fresh
SessionClock observer. Provider/verifier/trust state remains shared; account
for configuration vectors/headers, retained name and observer allocations in
M07e. Ordinary missing time is sticky only for that connection, even if a later
query succeeds. Shared source retirement overrides that local failure with
Crypto. No mutable transient error cell lives on the reusable configuration.
The observer checks ClockHandle health without invoking its callback, so it
cannot lose a transient failure by re-polling. Health waits for an in-flight
serialized callback before inspecting the source slot.

Every operational call samples supplied time even when the backend does not
request it. Every mutable backend call checks the observer afterward, including
successful calls; publication checks again. The TLS 1.2 ignored session-save
failure is therefore terminal before exposing Finished. A consuming unwind
boundary temporarily removes the whole live connection and restores it only
after successful work and fences. Error/unwind drops that state, invalidates
cached evidence, discards all pending output and fixes the error permanently.
Caller output buffers can have been touched: discard them and retained socket
write tails on error. Native abort/OOM, hooks, destructor failures and blocking
retain the earlier limits. Dropping native buffers is not a secret-erasure
claim, and the backend is never operated again after an unwind to attempt
cleanup.

Record framing checks the complete header/body, exact length and protection
state before backend intake. TLS 1.2 protection begins only after an accepted
ChangeCipherSpec; TLS 1.3 encrypted outer application-data records have their
separate limit. Visible pre-Finished alerts are refused. Partial native reads
loop over the one caller record with positive progress and an observer check
at each step. Unread plaintext blocks another record without consumption;
handshake output must drain first. Established sessions accept input while
output is pending, so simultaneous writers can make progress.

At most 16384 plaintext bytes may be queued per call, with one outstanding
application record. Incoming plaintext is bounded to the same size. The native
application buffer setting is 32 KiB. Drainable ciphertext initially permits
65536 protocol bytes plus the 18437-byte application wire reservation. After
successful Finished and complete facade drain of the final local handshake
flight, both roles permanently reduce the ciphertext ceiling to 36874 bytes,
one application and one protocol wire reservation. A later pending record
cannot restore the handshake allowance. A caller socket tail belongs to its
separate pump buffer. These are queue refusals, not a preallocation or
total-memory guarantee. Handshake, certificate and native allocations remain
separate qualification work. The
65535-byte deframer limit includes retained record headers: with 16384-byte
plaintext fragments, a deliberately malformed 65511-byte handshake payload
reaches decoding while one extra byte exhausts capacity. With 4096-byte
fragments that payload boundary is 65451. No 65535-byte payload promise follows
from the deframer size.

While the local write half remains open after Finished, an internal empty
native write flushes a deferred TLS 1.3 KeyUpdate response without accepting
application bytes; its output then enters the same accounting and clock
checks. This avoids waiting for application activity to send the response.
After local close, suppress that flush: no new control record may follow
close_notify. After appending the locally requested close, the remaining
output count becomes a decreasing baseline. Refuse any subsequent native
output growth, including successful TLS 1.2 HelloRequest processing that
would append a no_renegotiation warning. A backend-held deferred update in
that state still belongs in
M07e allocation accounting and is dropped when the connection retires.

Read/write operations before Finished return Blocked with no consumption.
Empty buffers otherwise return zero progress, except a fully drained closed
read reports Eof. Read and write closure are independent for TLS 1.3; TLS 1.2
receiving an alert with pending ciphertext or a caller write tail is terminal
Protocol, including simultaneous close when local writes are already closed.
Otherwise close_notify closes writes and queues its response.
Local close appends after pending output and is idempotent. Closing before
Finished is terminal Invalid; abort cancels handshakes. Writing after local
close is terminal Invalid and discards even a queued close. Transport EOF
without received close_notify fails Protocol. Framed input after peer close
is explicitly discarded; malformed framing still retires the session. Closed
means both halves closed and facade output drained; the caller must still
flush its own retained tail before completing transport close.

Tests use the public client session against local backend peers, including
seven-byte pipes, one-byte reads, short output tails, simultaneous full-size
writes, unread-plaintext backpressure, exact record/reassembly bounds, clock
failures, fixed redacted errors, key updates and version-specific closure.
Synthetic session unwinds prove state consumption and independent reuse of a
healthy configuration. Post-local-close malformed/bad-MAC cases qualify both
host and portable behavior without emitting a later alert. M07d/M07e must
still qualify mail adapters, resource leases and complete service bounds.

### Socket-free server admission

`TlsSession::server` retains one ServerConfig and the common per-session clock
observer. It begins in a private Acceptor with no authentication evidence.
Single-identity construction checks that identity; multiple-identity sessions
check material only after name selection. A native connection replaces the
Acceptor only after complete ClientHello validation and selected-key/date
checks, inside the same consuming unwind boundary as client progress.
Acceptor errors discard their native alert; failed sessions expose no output.

Before native intake, a streaming raw ClientHello checker validates plaintext
handshake fragments. Its state is at most 1024 bytes on qualified targets,
including fixed 253-byte name buffers; it allocates no memory and retains no
caller slice. It carries the four-byte handshake header, checked remaining
lengths and a field state across records. Check session-ID/cipher/compression
vector boundaries, exact extension lengths and one nonempty host_name entry
within a unique SNI extension. Unknown extension payloads are bounded skips;
native parsing still validates their semantics and protocol order. Refuse
IP literals and malformed DNS with the existing name policy before native
conversion can lose them. Accept absent SNI as absence, subject to routing.

Validate complete and fragmented initial/retry hellos. At most two ClientHellos
are admitted, with identical folded name or absence on retry. The checker runs
only after backpressure checks, so a Blocked record can be retried without
advancing it. Validate the entire incoming record before passing any of it to
the backend; an invalid final SNI fragment cannot trigger signing. TLS 1.2
ciphertext after accepted ChangeCipherSpec and TLS 1.3 outer application-data
records bypass this plaintext scanner. Existing record/deframer bounds remain
independent; no extra 64 KiB handshake pool is introduced.

At initial acceptance, require the raw checked name to equal the backend's
name, then select through the configuration's immutable routing table. Check
only the selected identity before converting Acceptor state into a signing
connection, before later handshake operations (including HRR) and again before
publishing Finished. A candidate configuration still cold-checks all material;
expired or retired unrelated identities within an existing configuration do
not block its healthy selected identity. Completed connections check selected
key health on every operation, without re-expiring their established peer
authentication when the certificate's date passes. Shared key retirement is
checked after native calls and before publication; ordinary peer errors retire
only the session. A private health helper performs the existing fenced public
point operation, without adding a signing or secret-export path.

Server configuration clones install their own sticky clock observer before
configuration-dependent native work. Sample supplied time before operations
and again after server work, including completion; backend callback failures
and shared source retirement remain terminal. Local key and clock fences are
independent. Unwinds consume Acceptor or connected state and cannot restore it.

Full Finished success produces Unauthenticated for ordinary inbound TLS. With
mandatory private-client authentication, require the verified native leaf and
hash its exact DER using owned SHA-256 into VerifiedClientLeaf. Native peer
certificate presence alone is insufficient. No certificate bytes, backend
object, gateway authorization or socket address crosses this session API.
The server uses the same progress, fixed errors, close behavior and output
growth refusal as the client. Mail authorization and transport still await
M07d; total generation/session/native memory qualification remains M07e.

Local fixtures exercise TLS 1.2/1.3 public sessions, tiny bounded pipes,
simultaneous full-size writes, clean close, private mutual authentication,
typed certificate refusals and independently checked leaf digests. A forced
allowed-group HRR first proves a complete valid handshake, then verifies raw
IP/changed-name refusal across fragmented retry records. Lifecycle fixtures
separate cold candidate refusal, selected/unrelated expiry, expiry during
handshake, established-date policy, shared-key retirement and clock/unwind
retirement. No live service or external network endpoint is used.

### Mutual TLS backend qualification

M07a5 extends the local backend fixture with mandatory client-certificate
verification under both TLS 1.2 and TLS 1.3. Each server verifier receives
an explicit provider and one generated local trust anchor. The client still
verifies the server's chain, time and localhost name. Valid clients complete
the handshake and send application data; the server's completed connection
reports the exact client leaf DER supplied by the fixture.
The check requires `!is_handshaking()` before using that leaf. Rustls can
expose peer certificates before checking Finished; presence alone is not
completed-handshake evidence. Refused client credentials leave no peer chain.
The mutual setup also refuses a wrong server name, and its client-expiry
assertion identifies both the verification time and the expired leaf's date.

For each protocol, refuse an absent client certificate, an unknown issuer,
an expired leaf, server-only extended key usage and a bad issuer signature.
A mismatched client certificate/private key is refused during configuration,
before connecting. Negative cases require the expected typed TLS error, not
an arbitrary fixture failure. The process-global provider remains unset.

The tested configurations disable client resumption, server session storage,
TLS 1.3 tickets and early data. For each version, three configuration pairs
cover both disabled, a resumption-enabled remote client, and a
resumption-enabled remote server. Two connections reuse each pair and require
full authenticated handshakes, qualifying each side's refusal independently.
This qualifies this explicit fixture
configuration; it does not configure a production session or prove an absence
of temporary constructor allocations. Work retains the portable harness's
existing wire, iteration, output and process-time bounds.

These tests establish backend client-certificate behavior only. They do not
supply an opaque TLS session API, filesystem/key admission, gateway peer/IP
allowlist authorization, STARTTLS transitions, certificate rotation or
resource qualification. A verified client chain or matching leaf hash is not
mail authorization; td-mta's adapter must combine completed-handshake evidence
with its current gateway policy. Generated certificates and in-memory peers
contact no external service and add no fixture dependency.

### Build, provider confinement and TLS policy

M03b1 pins versions, features, licenses and roots; M03b2 pins the portable
build inputs: the pinned host Rust kit and its musl target libraries, the
authenticated Debian build root, whose binutils inspect and split the
artifacts, and the checksum-pinned musl source archive, whose notice the
linked libc carries. Nothing in the closure compiles C. M03b2c implements the
isolated static musl build, packaging binary and clean-runtime smoke specified
in PORTABLE.md. M03b2d1 adds compiler-resolved API confinement;
M03b2d2 adds the bounded local TLS smoke specified in PORTABLE.md.
The aarch64 cross-build uses the separately pinned target library and the
build root's Debian aarch64 cross binutils described there. It checks
static ELF architecture
and API confinement but reports runtime untested; successful compilation
is not x86-64 runtime or resource qualification.
Reuse compatible reviewed pins without inheriting td-net's dependency set.
Rustls is a direct dependency of td-crypto and of td-tls-provider, with the
same pinned line and no built-in backend. td-primitives supplies bulk digests,
entropy, comparison, P-256 keys and signing and certificate key validation,
directly and, for TLS, through td-tls-provider. The owned SHA-256
implementation serves allocation-free streaming digests. Disable unused
features.
The portable musl artifact follows td-mta/DESIGN.md section 3; it does not
inherit the provenance of td's separate target image graph.

No selected package compiles C or links a system library; the exact
active-graph check refuses one that would. The portable build still plants
decoy compiler, OPENSSL_DIR, pkg-config and CMake inputs, so a build script
that reached for one fails. Missing declared inputs fail.

The portable build combines rustc's private-dependency check with a resolved
public API graph, including
nested/hidden exports, aliases, signatures, generic bounds and associated
types. Only local and std/core/alloc definitions are allowed. Opaque private
backend fields are allowed; exported macros and unsupported public item kinds
are refused. Conditional compilation using `doc` or `debug_assertions` is
forbidden in the qualified configuration, including macro-generated conditions,
so rustdoc cannot hide a release API. The compiler pass separately covers dyn
trait implementations absent from the graph. PORTABLE.md owns the pinned compiler,
configuration, limits and mutation fixtures. The M03a root-export doctests
remain small consumer checks; they do not establish the whole boundary.

All Rustls provider construction, TLS configurations, verifiers and key loading
stay private here. Supply the provider explicitly to every configuration and
verifier; never install or rely on the process-global default. M07 confines
those calls and uses a fresh-process test to verify the default remains unset.
The test may inspect that default; public and production APIs may not expose it.

M07 supplies opaque TLS configurations and bounded session progress through
td-owned types and caller buffers. No mail-specific authorization is delegated
to this crate. td-mta owns sockets, scheduling, deadlines, resource leases,
STARTTLS framing/reset, account/gateway policy and durable state. Its transport
adapters call this facade. This crate owns TLS records, handshakes and generic
chain/time/name/client-certificate verification. Verification results distinguish
verified identities from raw peer claims; td-mta applies gateway allowlists
before reporting gateway authorization. There is no ignore-verification escape.

TLS.md records the accepted sets and preference orders for TLS versions,
cipher suites, key-exchange groups, certificate and handshake signatures, and
local/peer key formats. M07 must implement and test those explicit lists;
provider defaults cannot silently expand the baseline. Dependency or backend
changes compare against it. Persist standard keys/certificates/digests, never
provider contexts or backend IDs. Retain non-secret golden key fixtures,
including generated,
accepted and rejected PKCS#8 variants and inconsistent embedded public keys.
Replacement reconstructs contexts after restart and tests key interchange and
rollback in both directions; "td-primitives backend qualification" below holds
those fixtures. Algorithm, trust or format changes need their own explicit
compatibility handling.

## Qualification and later owned primitives

M07 implements the provider conformance suite here, plus mail integration
fixtures in td-mta. Test SHA-256 chunk boundaries/known answers, fixed-length
constant-time comparison's functional results, public-point known answers,
key generate/load round-trips, malformed-key rejection, capacity/error mapping
and fault-injected entropy failures. Verify generated ES256 signatures as raw
64-byte r || s, not DER, with an independent test-only verifier qualified by
known valid and invalid vectors. Cross-backend tests compare verification,
never randomized signature bytes. Pin and approve external vectors/tools
before adding them. TLS tests cover the configured policy and malformed/refused
peers. All endpoints are local.

Every backend change must requalify allocations, peak stack and
whole-process RSS on the supported artifact/CPU paths; with no C in the
closure, all heap goes through Rust's allocator, which the allocation probe
counts. td-mta/RESOURCES.md owns the service budgets;
wrapping a library does not establish those bounds. Warm crypto-using workers
before admission. Document and verify secret ownership, wipe-on-release and
its limits, including provider internals and compiler-created copies. Drop or
Vec::clear alone does not prove erasure. An allocator hook
or any other new unsafe surface first requires the normal UNSAFE.md amendment.

A Result wrapper cannot contain an abort. The random source refuses rather
than falls back; there is no weaker randomness or partial-byte fallback, but
OOM and abort still terminate the process. Callers must recover durable state
after process death. Test returned-error
handling separately from process-death recovery and report which fatal paths
were audited/exercised. Do not claim arbitrary provider panic/OOM survival.

td-owned primitives implement the private backend behind this same facade;
they live in `td-primitives` (AGENTS.md principle 2), and they introduce no
public mail dependency. The Rustls provider bridge is the shared
`td-tls-provider` crate, td-net's provider and admitted by the same policy,
since code two crates need lives in one crate (principle 2). This crate
assembles its and TLS.md's policy from it inside private fields and never
re-exports it, so the bridge stays private to the facade. Retaining the Rustls
TLS engine means the crate is not std-only. Reuse of the existing engine
SHA/Ed25519 and td-fido P-256 implementations confers no unproven
side-channel claim and does not migrate those consumers into this
external-dependency crate. Cross-component primitive consolidation has its
design in `td-primitives/DESIGN.md`. td-mta/IMPLEMENTATION.md F04 owns mail
integration staging. Each primitive needs independent cryptographic and
exact-artifact side-channel review, including compiler/flags/target, nonce
generation and entropy/optimization-barrier choices; passing functional tests
alone is insufficient. td-primitives/DESIGN.md states the source-level
claims and what is not claimed. Do not prebuild unused primitive interfaces.

Side-channel review scope. A model-run side-channel review (Claude Opus
5.5) of the secret paths td-crypto uses (td-mta/IMPLEMENTATION.md F04 step 2
lists them; private-key PEM decoding included) found no secret-dependent
branch, secret-indexed load or nonce flaw. It covered their source and the
disassembly of the crypto-cargo gate's host build and of the portable td-mta
artifacts PORTABLE.md's builds produce for x86_64-unknown-linux-musl and
aarch64-unknown-linux-musl: pinned Rust 1.99.1 and LLD 23.1.1, the release
profile, `target-cpu=x86-64` and `generic` respectively, `+crt-static`, frame
pointers and `debuginfo=1`. It measured nothing: no timing, power,
electromagnetic or microarchitectural leakage. It assumes data-independent
timing of AESENC, PCLMULQDQ, MUL and UMULH and the conditional moves and
selects, which on aarch64 holds without PSTATE.DIT only by core
implementation. It does not cover rustls's internals, and it holds for that
toolchain, those flags and those targets only. No gate asserts the compiled
form of the constant-time routines, so a change to any of them needs the
review again. The independent cryptographic review F04 requires has not
taken place.

Selected-key health currently uses the owned key's mutex-backed public-point
operation. A server session checks it before work, after backend calls and at
publication, so established record work can wait behind another session's
signing operation. These repeated acquisitions preserve shared retirement
semantics; they are not qualified as nonblocking or latency-bounded. M07e must
measure contention across handshake and established workers, along with the
shared clock callback, before service admission. Any later lock-free health
optimization must independently preserve those retirement fences.

### td-primitives backend qualification

AWS-LC was this crate's backend until td-primitives replaced it. What it
wrote and checked is kept as fixed known answers in `src/fixtures/`, read by
`backend_tests.rs`, so compatibility with keys and signatures made under it
needs no AWS-LC at test time:

- `aws_lc_keys.txt`: four P-256 keys AWS-LC generated, as its 138-byte PKCS#8
  documents, with their public points and AWS-LC's raw and DER signatures.
  Each loads here in all four accepted shapes to AWS-LC's public point,
  AWS-LC's signatures verify under td-fido's independent verifier and
  td-primitives, and this backend's signatures with the key verify under the
  independent verifier.
- `td_keys.txt`: four keys in this backend's generated shape, from
  td-primitives scalars, with td-primitives' DER signatures. When they were
  written AWS-LC loaded each to the same public point and verified each
  signature, so a key generated here loads under the backend it replaced;
  the test holds that a freshly generated document has that same layout,
  and that td-tls-provider's key provider loads it.
- `aws_lc_signatures.txt`: AWS-LC's signatures under each certificate
  algorithm it signs. With td-tls-provider's big-integer rows
  (`td-tls-provider/tests/signature_answers.txt`), each must be taken by
  exactly its own algorithm's policy verifiers (PKCS#1 also under the
  identifier without NULL parameters), with an EC key uncompressed or
  compressed but never in another form, refused after a changed message or
  signature byte, and no refused row may be taken by any verifier: the
  answers AWS-LC gave for the same inputs.
- `certificate_paths.txt`: twenty AWS-LC-signed paths, a P-256 leaf under a
  root for each algorithm it signs with and each EC root's key in both point
  forms, which verify under the policy's certificate algorithms, and fail
  with a changed leaf signature; and `ml_dsa_path.txt`, an ML-DSA-44 path
  AWS-LC accepted, which the policy refuses.

The loader test holds the scalar and embedded-point checks pkcs8 parsing
leaves to it: scalars 0, n, n + 1 and all ones are Invalid in every shape, 1
and n - 1 load, and another key's embedded point is Invalid, as AWS-LC
refused them. Policy assembly tests pin every suite, group, algorithm
identifier pair and mapping, each TLS 1.2 suite's signature schemes and each
suite's confidentiality limit. Local handshakes in both roles cover the TLS
1.3 and ECDHE-ECDSA suites, every group and the ECDSA P-256 scheme, the only
key this crate signs with.

`handshake_peer_tests.rs` runs this crate's own sessions, as a client and
as a server requiring client certificates, against a rustls peer whose
handshake signatures come from arithmetic in that file sharing no code
with td-primitives: RSA PKCS#1 v1.5 and PSS, ECDSA on each admitted curve,
Ed25519, SHA-384 and SHA-512. Its keys are `src/fixtures/handshake_keys.txt`,
written by `handshake_keys.py`. As client, this crate completes every
admitted scheme under each version and suite kind that allows it; as
server, its P-256 identity fixes one suite per version, under which it
takes every admitted client-proof scheme. Both roles refuse a corrupted
signature, a scheme the key does not fit, the wrong hash, padding or PSS
salt length, and a scheme not offered, pinning this crate's error and
plain rustls's for the same handshake. TLS 1.2's curve rule is TLS.md's
and is pinned as stated. Only the signing is independent: key exchange,
AEAD and transcript hash are td-tls-provider's, rustls runs both ends, and
the certificates are td-primitives' P-256 signatures. The hashes and
Ed25519 have known answers there; the ECDSA and RSA encoders have none of
their own, so their correctness rests on td-primitives' verifiers, which
are anchored on AWS-LC's answers, accepting them.

Certificate key admission has
known answers built with Python's big integers
(`src/fixtures/canonical_keys.py`): valid, off-curve, out-of-range,
non-residue, hybrid, infinity and truncated EC points in both forms, with
compressed and uncompressed aliases comparing equal, and RSA keys at and
past each modulus and exponent bound.

The same tests run on the portable musl artifact as named smoke cases
(PORTABLE.md); its aarch64 cross-build compiles them. This is functional
evidence. Allocation, stack and RSS evidence is in td-mta/RESOURCES.md and
PORTABLE.md; F04's independent cryptographic and exact-artifact
side-channel review and the M07 mail/TLS service integration remain open
(td-mta/IMPLEMENTATION.md F04).

### Exact issued DNS identifiers

`ServerIdentity::check_exact_dns_names` checks that the leaf SAN contains
exactly the identity's admitted bindings, once each, using ASCII case folding.
It refuses wildcards, additional DNS names and non-DNS GeneralNames. The
existing bounded DER/SAN reader supplies the bytes; no alternate certificate
parser is introduced. Ordinary externally provisioned identities retain their
existing wildcard/name-coverage semantics unless the caller requests this
additional check. ACME publication requires it before selecting a generation.
This checks identifiers, not public-root trust; the issuer response still needs
an authenticated configured CA transport.
