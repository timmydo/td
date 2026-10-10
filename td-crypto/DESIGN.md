# Cryptography and TLS boundary

## Status and ownership

This crate owns td's provider-independent cryptographic API. The compiling
M03a surface contains `Error`, `Entropy`, `Digest` and `Crypto`, extracted
from td-mta's existing ports. M03b1 admits the private Rustls/AWS-LC
dependencies and checks their offline host build. M07a1 implements opaque
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
malformed/tampered-record refusals in the isolated static executable. Mocks
and interface tests are not cryptographic conformance evidence.

The target dependency graph is:

```text
td-mta (library and packaging executable)
  -> td-header (std-only bounded header lexical cursors)
  -> td-json (std-only bounded JSON string framing)
  -> td-mime (std-only MIME/header/charset/Unicode implementation)
  -> td-nfc (std-only bounded canonical composition)
  -> rusqlite (private bundled SQLite body/metadata backend)
  -> td-crypto (td-owned API and private backend)
       -> rustls + aws-lc-rs + reviewed root data
```

td-mta/DESIGN.md section 3 owns the direct mail dependency rule. After backend
admission, td-mta is this crate's only permitted roster consumer. The common
gate must reject other direct or transitive consumers; the engine workspace
and other std-only roster crates cannot inherit its external closure.

The mail crate also has the explicitly approved private rusqlite 0.40.2
closure with bundled SQLite 3.53.2. td-crypto does not consume SQLite.
The common offline vendor uses the td-mta lock's union of crypto and SQLite
sources, checking every archive checksum; td-crypto's selected 19-package
normal/build graph is unchanged. Mail adds exactly seven selected packages,
listed in DEPENDENCIES.md, with no other roster consumer. The exact direct
manifest and active graph are pinned alongside the existing crypto policy.
Mail's STORAGE.md owns native SQL, limits, filesystem and recovery semantics.
The shared host and portable drivers force bundled linking and reviewed
LIBSQLITE3_FLAGS; ambient LIBSQLITE3_/SQLITE_/SQLITE3_ controls are refused.
No pkg-config fallback, extension loading, bindgen or external database tool
is selected. Native SQLite heap/stack/RSS and target qualification are new
mail obligations, not established by earlier crypto-only measurements.

`td-crypto` does not depend on td-mta, its identifiers, configuration grammar,
mail protocols, storage, logging, workers or service administration. Its own
code uses std and follows the panic/indexing and unsafe contracts. The initial
backend uses the approved Rustls/AWS-LC category. AGENTS.md and the shared
host/sandbox gate admit only the exact named manifests, locks, root Cargo
configuration and selected normal/build features in
`builder/src/crypto_policy.rs`. Other roster closures remain std-only.
Changing any pinned input requires an explicit policy update. The
checkout's root `.cargo/config.toml` is required. Ancestor
`.cargo/config.toml` files are accepted only when each matches that same
compiled pin, allowing nested worktrees to use their own runner. Legacy
`.cargo/config` files remain refused at every level. Automatic build.rs
files are refused for all seven local source packages: td-civil,
td-crypto, td-header, td-json, td-mime, td-nfc and td-mta. Their exact manifests and locks are
pinned; td-civil, td-header, td-json, td-mime and td-nfc remain std-only roster
crates. The Cargo config pin contains only target runner settings, for
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
these two crates. This wrapper verifies private archive copies, reconstructs
and rehashes extracted sources before cache reuse, uses a fresh Cargo home,
forces offline/frozen source replacement, and checks Cargo's selected graph.
A missing, stale or tampered vendor fails; Cargo never fetches a substitute.
Repository Cargo configuration cannot set AWS_LC_SYS controls, including
through escaped keys or inline environment tables.
The wrapper qualifies the provisioned host Rust/C toolchain. Host invocations
share `.td-build-cache/crypto-target` unless CARGO_TARGET_DIR is supplied.
Verified archive extraction and tree hashing repeat for each invocation; a
persisted digest alone cannot authenticate changed source bytes. Its build is
not the portable artifact: M03b2 must separately pin all portable native and
Rust inputs and prove the clean musl build before M03 completes.

## Public API and failure rules

Public functions, traits, errors and opaque handles use only std types and
types defined by td-crypto. Never re-export upstream modules, type aliases,
provider configuration types or error chains; a public function returning a
Rustls type would breach the boundary even without a direct consumer dependency.
Concrete backend state lives in private fields/modules. There is one selected
implementation per operation in source, with no feature, environment or runtime
backend selector.

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
to an unwarmed thread. This warms only the RNG path used by this API; other
crypto/TLS paths still require their own qualification. Empty fills succeed
but do not initialize native state. No Default constructor bypasses warm-up.

The opaque handle owns no Rust heap buffer. Native per-thread state survives
handle drop. Initial use can allocate RNG state, a pthread pointer table and
source-specific state; moving a Rust value would not transfer those allocations.
There is also lazy process-wide state. The admitted Linux build includes jitter
entropy; absent the native VM-generation-detection alternative, it initializes
a global seed DRBG and jitter collector with its memory buffer/self-test.
OS-randomness setup, VM-generation probing and the fork-detection mapping also
have process-wide costs. Each worker's tree-seed initialization and later
reseeds take a shared global lock; reseeding can collect jitter entropy while
holding it. First use in the process costs more than later worker warm-up.
M07 must measure both global and per-worker memory, stack and lock contention.
Constructor success establishes neither a no-allocation hot path nor a bound
on later native calls or elapsed time.

The admitted non-FIPS `aws_lc_rs::rand::fill` path calls native `RAND_bytes`
and maps its return status without a Rust unwrap/expect path. The pinned
native function returns one on every normal return. Allocation, entropy,
initialization/reseed/generation, lock/once and state-validation failures can
abort the process. Those failures cannot become `Error::Entropy` or be caught
by Rust unwinding. There is no weak-randomness or partial-byte fallback.

Native calls can also block: early-boot OS entropy initialization may wait
without a deadline, and calls after exit-time zeroization can remain locked
forever (or abort when a lock reports an error). Waiting/fatal native paths
can write diagnostics directly to stderr, including the executable path from
AT_EXECFN; td's fixed returned errors do not redact that separate channel.
M07 must qualify startup/shutdown ordering and supervision around these limits;
this constructor supplies no timeout or cancellation mechanism.

An ordinary returned provider error maps to fixed `Error::Entropy` and
clears the entire caller slice. Callers must still discard it; zeroed bytes
are never successful entropy. This is observable hygiene, not guaranteed
erasure of compiler/caller copies. Private callback tests inject a returned
error after a partial write at the same boundary; they do not simulate native
entropy failure, OOM or process-death recovery. A local sample comparison is
only a no-op/constant-output wiring check, not an entropy-quality test.

Normal pthread teardown invokes native entropy-source cleanup and frees its
source/RNG allocations through the default cleansing allocator. Dropping the
handle does not trigger it; normal process exit does not run the main thread's
pthread key destructors. Exit destructors clear registered frontend DRBGs,
lock against later output and invoke source-specific zeroization. The tree-seed
DRBG path reseeds with OS data (zeros if entropy is unavailable), rather than
simply clearing its bytes. A separate destructor frees the global seed DRBG
and jitter collector. Some thread nodes remain for OS reclaim at exit,
including the main thread's node and nodes racing shutdown. These destructors
are normal-exit paths; `_exit`, abort and kill can bypass them. This audit
supplies no all-path erasure claim for source-owned storage, stack temporaries
or compiler/caller copies.

### Streaming SHA-256

`Sha256::try_new` creates one inline td-owned operation with eight chaining
words, one 64-byte partial block, a checked byte counter and terminal-state
flag. Construction, updates and consuming finish use no heap, native provider,
locks or OS calls. The fallible public API remains unchanged. The concrete
`Crypto` factory delegates to this constructor. Only this direct streaming
operation uses the owned primitive; Rustls and ES256 signing still use AWS-LC.

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
catch; the crate still requires unwinding for its remaining native adapters.

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
fragmentation and every split through 257 bytes against the admitted AWS-LC
implementation. Engine-derived code is not an independent oracle for this
replacement. Synthetic counters exercise the limit without hashing exabytes;
counter differences also check that each upper padding-length byte matters.
Malformed private-state fixtures check fail-closed retirement. The portable
harness runs these cases and the existing mail-format digest fixtures. Its
inline state is at most 128 bytes; source and exact-artifact call-graph review
qualify the no-allocation and content-independent control/address claims for
the recorded compiler/flags/target. Repeat artifact review when those change.
This does not complete whole-service stack/RSS qualification or qualify other
primitives, targets or microarchitectural leakage outside that review scope.

### Crypto factory and P-256 signing keys

`Provider` is a stateless td-owned implementation of `Crypto`; creating it does
not initialize native state or warm a worker. Its SHA-256 factory delegates to
`Sha256::try_new`. Fixed 32-byte equality calls the admitted provider's
constant-time comparison (`CRYPTO_memcmp`), with no ordinary-equality fallback.
Functional comparison tests do not establish exact-artifact timing behavior.

`P256Key` owns the backend key behind a private mutex. Public-point extraction
and signing serialize on that handle. Each operation takes ownership of the
native key inside the same narrow unwind boundary; only success restores it.
A returned operation error or Rust unwind drops the key and leaves the handle
retired. Later operations fail Crypto; a poisoned mutex also refuses access.
No clone, private-scalar export or backend handle is exposed. The mutex can
block; these operations have no timeout/cancellation mechanism. M07 must fit
actual usage into the control-worker lifecycle before enabling service.

Key input is one complete unencrypted PKCS#8 v1 DER PrivateKeyInfo (version
integer zero), capped at `P256_PKCS8_CAPACITY = 150` bytes. The outer algorithm
must contain exactly id-ecPublicKey and named-curve prime256v1 OIDs. Its private
OCTET STRING contains one SEC1 ECPrivateKey sequence: version one, exactly
32 scalar octets, optional explicit [0] containing the same curve OID, then
optional explicit [1] containing a zero-padding BIT STRING with a 65-byte
uncompressed point. All lengths use minimal definite DER encoding; every
container must end exactly. The four accepted shapes are 67, 79, 138 and 150
bytes, depending on the two optional inner fields. The provider validates
scalar range, curve membership and embedded-public-key consistency, deriving
the public point when omitted. Structural acceptance alone is no key proof.

The DER entry point refuses encrypted keys, PEM, bare SEC1, PKCS#8 v2,
attributes, explicit curve parameters, compressed/hybrid/infinity encodings, overlong/indefinite lengths,
wrong/duplicate/reordered fields and trailing bytes at every level. These are
intentional subset restrictions. The bounded precheck is needed because the
pinned provider's outer PKCS#8 reader does not reject trailing input itself.
The formats follow [PKCS#8](https://www.rfc-editor.org/rfc/rfc5208) and the
[ECPrivateKey structure](https://www.rfc-editor.org/rfc/rfc5915); curve
parameters may be inherited from the outer algorithm for compatibility with
the provider's conventional PKCS#8 encoding.

Generation requires caller capacity of at least 150 bytes before invoking the
provider. The pinned provider emits a 138-byte document with an inner public
point and curve parameters only in the outer algorithm; validate it against
the same accepted subset before copying. Return the used length and leave the
unused tail unchanged. Public-point output is exactly 65-byte SEC1; ES256
output is exactly 64-byte r || s. Signing hashes the complete message with
SHA-256 once; it accepts no prehashed convention. Refuse messages beyond the
SHA-256 bit-length limit. Output buffers remain unchanged on returned errors;
callers must treat them as having no result. Caller-owned private-key input
and generated output remain the caller's secret-lifecycle responsibility.

Generation, parsing, public-state construction and signing can allocate native
and Rust memory. The pinned signing path constructs digest/signing contexts
and temporary signature buffers for each call. This is not qualified for a
no-allocation hot path. The compatibility SecureRandom argument to native key
generation/signing is ignored by the pinned provider; those calls use native
RNG state, with the blocking/abort/stderr limits above. A synthetic Rust error
or unwind injection is not a native RNG-failure test.

The native key and its public view hold references to the same EVP key; their
final release frees the wrapped scalar through the default cleansing allocator.
Generated provider Document storage uses its zeroizing destructor; temporary
native encoding buffers use native cleanup. This does not guarantee erasure
of stack/compiler copies or cleanup after abort/kill. Ordinary parse failures
map to Invalid; generation/signing failures and caught Rust unwinds map to
Crypto. No native diagnostics or panic payload becomes a public error. The
whole-graph unwind and hook limitations below apply.

This boundary requires Rust unwinding throughout the target graph, including
the final executable. The crate refuses its own compilation with `panic=abort`;
that check alone cannot detect linking an unwind-built rlib into a separately
compiled aborting executable. The supported portable Cargo build compiles the
whole graph with the default unwind strategy. Every future target recipe or
consumer build must preserve that strategy through final linking; a final-only
abort override is unsupported. Panic hooks still execute, and an aborting or
panicking hook is outside the boundary. Native aborts, OOM and unwinding failures are not contained.
P-256 failure tests inject Rust unwinds at its private operation boundaries;
they do not establish native allocation-failure or entropy-failure recovery.

Tests use only existing repository crypto material: the engine SHA-256 and
td-fido P-256 source/vector fixtures are compiled as test-only independent
oracles for the native ES256 signer. Their own known-answer, valid/invalid
signature and arithmetic cases
qualify the oracle; generated signatures must verify against the message,
not its digest, and modified messages/signatures are refused. No randomized
signature-byte equality is required. These sources never enter the shipped
backend or add a Cargo dependency. Portable staging includes the exact oracle
source files and vector data in its source digest. Functional, grammar and
failure qualification does not complete native allocation, timing, stack/RSS
or service integration work.

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
bypass cleanup. Backend key construction still allocates and retains the
native failure limits above.

Tests compare certificate decoding with the already admitted backend's PEM
reader over padding, byte-value and DER-length boundaries; local generated
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

The certificate algorithm selector checks all nineteen exact public-key and
signature AlgorithmIdentifier pairs against the pinned backend prefix,
including parameters, before returning its static slice. The three excluded
ML-DSA pairs and their order are also checked.
The backend must still have its reviewed 22-entry inventory; changes refuse
with Crypto.
No ML-DSA entry or global provider is inherited. Constructing that backend
inventory creates temporary cold allocations; this is not an allocation-free
configuration path. The same private provider also validates handshake mappings; complete
configuration policy remains M07b3.

Reject duplicate or misordered certificates, and repeated issuer
subject/key pairs even when their certificate bytes differ. The key
comparison uses native-parsed, reserialized SubjectPublicKeyInfo, so
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
error source. Native abort/OOM, panic hooks and backend blocking retain the
limits above. No secret erasure, timing or whole-service resource guarantee
follows from a returned Result or a synthetic unwind fixture.

Owned vectors reserve checked bounded capacities; the key and native
verification code still allocate. M07e must charge inputs, decode scratch,
certificate/name copies, temporary native key parsing/canonical public-key
copies, backend inventories/path state, retained keys and overlapping
generations before the service can use this handle.
No listener or outbound adapter is enabled by M07b2a.

### Owned-key TLS signing bridge

Local identity admission retains its chain in one private shared CertifiedKey
and its existing opaque P256Key behind an Arc. The private TLS adapter retains
that same key, with no private-key export, reload or independent failure state.
Certificate byte vectors move into the retained chain without copying their
contents. Public inspection still returns the same certificate bytes and
counts. A cached 91-byte P-256 SPKI contains only public key material; native
key/leaf matching checks it before publication. No backend handle crosses the
public API, and no reference cycle retains an identity.
ServerIdentity retains Send, Sync, UnwindSafe and RefUnwindSafe. The erased
backend signing trait does not express the last two; the owned identity
implements them because its sole adapter has immutable public state and the
existing mutex-fenced, retiring key. Tests pin the concrete adapter's inferred
unwind traits. This is not a promise that native aborts or panic hooks recover.

The adapter advertises only ecdsa_secp256r1_sha256 and passes the unhashed
handshake message into the existing SHA-256 ECDSA operation. It converts the
64-byte r || s result into canonical positive DER INTEGERs and a SEQUENCE of
at most 72 bytes. Leading zero octets are removed, sign padding is inserted
when needed, and a zero scalar is refused as an internal failure. The native
signer supplies valid scalars; this private encoder is not a separate public
signature-validation interface.

Signature construction and its fallible Vec reserve run inside the same
locked operation/unwind boundary that owns the native key. A transform error
or unwind drops the key before restoration; all adapters/configurations
sharing it refuse later signing. The raw ES256 API uses that same boundary
with an identity transform, preserving its fixed output and error semantics.
A completed operation cannot retract signature bytes already returned; future
sessions must still recheck shared-key health at the TLS.md admission and
handshake-evidence publication points. Cached public bytes and certificate
inspection are not proof of live key state. Ordinary remote protocol/name
refusals do not retire the shared key.

Only fixed td-owned TlsError tags travel through the private native error
channel. Public errors never include native diagnostics, certificates,
signatures, names or secrets. Debug for the adapter/signer is fixed redacted
text. Rustls selector boxing and the returned signature Vec are TLS
allocations; chain/header vectors, SPKI and Arc ownership are cold generation
costs. M07e still must measure them with native signing/entropy costs. Native
abort/OOM, panic hooks, blocking and secret-erasure limits remain unchanged.

Tests cover canonical DER integer boundaries, native ASN.1 verification and
hash-once behavior, wrong scheme refusal, synthetic post-sign transform errors
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
P-384, P-521, Ed25519 or RSA 2048..8192. Parse the key with the admitted native
safe API during construction after exact raw-format checks. EC keys require
SEC1 points: uncompressed 04 with 65/97/133 bytes, or compressed 02/03 with
33/49/67 bytes for P-256/P-384/P-521. Ed25519 requires exactly 32 bytes; RSA
uses the minimal PKCS#1 public encoding already checked above. Nested SPKI,
trailing bytes and incompatible encodings are Invalid. Local identity
issuers share these checks; the local P-256 leaf remains uncompressed only.
Native key parsing is not a peer signature proof. The certificate's unused
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
all return Invalid. Allocation-reserve failures return Capacity; native
reserialization failures and caught Rust unwinds return Crypto. No peer
Verification result is produced by trust construction.

Cold construction catches Rust unwinds and drops unpublished state. Checked
vector reserves, decode scratch, native public-key parsing and owned anchor
copies remain part of the future M07e generation allocation qualification.
Native abort/OOM/blocking and unwind-hook limits remain unchanged. Tests use
disjoint generated CAs to prove complete replacement, ownership after input
reuse, atomic refusal, caps, supported key families, constraints and fixed
public inventory. A private client-auth verifier fixture proves certificate
path usage only, not TLS Finished or gateway authorization. All cases also
run in the portable artifact; no listener is enabled by this increment.

### Explicit TLS algorithm provider

One private constructor supplies TLS.md's exact nine cipher suites, three
classical key-exchange groups, nineteen certificate algorithms and ten
handshake signature mappings. Cipher/group lists are constructed explicitly.
The certificate selector checks the full native inventory, including
excluded entries. The handshake selector checks all thirteen native
mappings, each scheme and every mapped algorithm identifier pair in order,
then retains only the ten classical mappings. This pins the first ECDSA
verifier used by TLS 1.3 as well as the alternatives available in TLS 1.2.
These checks pin identifiers and ordering, not verifier behavior such as RSA
modulus bounds; the reviewed source/lock pins retain that behavior. Any
signature identifier inventory drift returns Crypto; no global provider is
installed or consulted.

Local certificate admission uses this shared provider's certificate list.
The local backend TLS fixtures now use it for both endpoints; designated
remote fixtures can still use native defaults to prove exclusion. Inventory
fixtures test truncated, expanded, reordered and substituted mappings,
including the three excluded ML-DSA mappings. A valid ML-DSA-signed leaf
passes the native certificate baseline and fails the selected certificate
policy. Local handshakes exercise all three classical groups under TLS 1.2
and 1.3, each TLS 1.3/ECDSA TLS 1.2 cipher, hybrid-only peer refusal in both
directions and refusal of a native ML-DSA server with a successful native
client baseline. Inventory tests alone cover RSA suites and P-384/P-521/
Ed25519 signing schemes; local positive handshakes use a P-256 signer.

Construction catches Rust unwinds and publishes no partial provider. Cold
native-default vectors and replacement vectors allocate; the native entropy,
key, abort/OOM and hook limits are unchanged. This is an algorithm policy
component, not a configuration handle, key-signing bridge or session API.
Version selection, role policy, SNI, ALPN, clock, resumption and resource
qualification still require their following increments before service use.
The native key loader and random source are retained backend components, not
restricted by these algorithm lists. Test-only configurations may load
native keys, including the excluded peer fixtures. Future production
configurations must use the owned P-256 signing bridge and must never call
the native arbitrary-format key loader through with_single_cert or
equivalent APIs.

### Outbound configuration and shared clock

`ClientConfig::new` constructs one immutable outbound configuration from an
admitted TrustStore, shared ClockHandle and TlsProtocol (Http1 or Smtp). Its
public API contains no backend types. Construction checks time once, copies
exactly the supplied anchors into the private verifier, and publishes no
configuration on failure. An explicit private store completely replaces the
public roots. There is no client identity, OS trust read, system-clock read,
network operation or provider default. Client configuration construction
alone authenticates no peer and exposes no session operations.

The configuration uses the explicit algorithm provider, TLS 1.3 followed by
1.2, mandatory TLS 1.2 EMS, outbound SNI and selected-ALPN checking. Http1
supplies only http/1.1; Smtp supplies no ALPN. Client resumption, early data,
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
Http1 selects only http/1.1, accepting absence; Smtp selects no ALPN. Native
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
native build inputs.
M03b2a implements checksum-pinned x86-64 musl header preparation as specified
in `PORTABLE.md`. It invokes no compiler or upstream script. M03b2b adds the
pinned host Rust kit and retained td GNU recipe outputs. M03b2c implements the
isolated static musl build, packaging binary and clean-runtime smoke specified
in PORTABLE.md. M03b2d1 adds compiler-resolved API confinement;
M03b2d2 adds the bounded local TLS smoke specified in PORTABLE.md.
Reuse compatible reviewed pins without inheriting td-net's dependency set.
Rustls and aws-lc-rs are direct dependencies only of td-crypto, resolving one
AWS-LC version pair for entropy, comparison, P-256 signing and TLS. The owned
SHA-256 implementation serves direct streaming digests only. Disable unused
features and all undeclared build fallbacks.
The portable musl artifact follows td-mta/DESIGN.md section 3; it does not
inherit the source-bootstrap provenance of td's separate target image graph.

Use the checked source/cc build path. Disable system libcrypto discovery and
fallback to CMake, bindgen or prebuilt NASM; put the pinned version's controls
in build wiring. For the interface using these names, force
AWS_LC_SYS_USE_SYSTEM=0 and AWS_LC_SYS_CMAKE_BUILDER=0. Reject incompatible
overrides. Plant decoy OPENSSL_DIR/pkg-config/CMake inputs and prove none is
consumed. Missing declared inputs fail.

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
rollback in both directions. Algorithm, trust or format changes need their own
explicit compatibility handling.

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

The initial backend and every replacement must qualify allocations, native C
allocations, per-thread RNG state, peak Rust/C stack and whole-process RSS on
the supported artifact/CPU paths. td-mta/RESOURCES.md owns the service budgets;
wrapping a library does not establish those bounds. Warm crypto-using workers
before admission. Document and verify secret ownership, wipe-on-release and
its limits, including provider internals and compiler-created copies. Drop or
Vec::clear alone does not prove erasure. An allocator hook
or any other new unsafe surface first requires the normal UNSAFE.md amendment.

A Result wrapper cannot contain a native abort. AWS-LC's fatal RNG paths can
terminate the process; there is no weaker randomness or partial-byte fallback.
Callers must recover durable state after process death. Test returned-error
handling separately from process-death recovery and report which fatal paths
were audited/exercised. Do not claim arbitrary provider panic/OOM survival.

Future td-owned primitives replace the private backend within this same crate;
they do not introduce another public mail dependency. The Rustls provider bridge
also stays private here. Replacing AWS-LC still retains the Rustls TLS engine;
it does not make the entire crate std-only. This crate owns the inventory of
existing engine SHA/Ed25519 and td-fido P-256 implementations and their audit
evidence before reusing any code. Reuse confers no unproven side-channel claim
and does not migrate those consumers into this external-dependency crate.
Cross-component primitive consolidation would require a separate design;
it is outside F04. td-mta/IMPLEMENTATION.md F04 owns mail integration staging
and cutover. Each primitive needs
independent cryptographic and exact-artifact side-channel review, including
compiler/flags/target, nonce generation and entropy/optimization-barrier choices.
Passing functional tests alone is insufficient. The direct streaming SHA-256
operation is an independently qualified early
cutover from F04: its native Context adapter is removed atomically, while
AWS-LC remains required for entropy, comparison, P-256 and Rustls. Further
candidates stay test-only until their qualified cutover; remove each obsolete
adapter and remove native dependencies/build inputs when their final user
is replaced. Do not prebuild unused primitive interfaces.

Selected-key health currently uses the owned key's mutex-backed public-point
operation. A server session checks it before work, after native calls and at
publication, so established record work can wait behind another session's
signing operation. These repeated acquisitions preserve shared retirement
semantics; they are not qualified as nonblocking or latency-bounded. M07e must
measure contention across handshake and established workers, along with the
shared clock callback, before service admission. Any later lock-free health
optimization must independently preserve those retirement fences.
