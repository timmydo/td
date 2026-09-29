# Cryptography and TLS boundary

## Status and ownership

This crate owns td's provider-independent cryptographic API. The compiling
M03a surface contains `Error`, `Entropy`, `Digest` and `Crypto`, extracted
from td-mta's existing ports. M03b1 admits the private Rustls/AWS-LC
dependencies and checks their offline host build. M07a1 implements opaque
streaming SHA-256 through the private AWS-LC backend. Entropy, signing-key
operations and TLS sessions remain unimplemented. Test-only backend qualification covers explicit
provider construction, SHA-256, local TLS 1.2/1.3 data exchange, certificate
verification and malformed/tampered-record refusals in the isolated static
executable.
Mocks and interface tests are not cryptographic conformance evidence.

The target dependency graph is:

```text
td-mta (library and packaging executable)
  -> td-crypto (td-owned API and private backend)
       -> rustls + aws-lc-rs + reviewed root data
```

td-mta/DESIGN.md section 3 owns the direct mail dependency rule. After backend
admission, td-mta is this crate's only permitted roster consumer. The common
gate must reject other direct or transitive consumers; the engine workspace
and other std-only roster crates cannot inherit its external closure.

`td-crypto` does not depend on td-mta, its identifiers, configuration grammar,
mail protocols, storage, logging, workers or service administration. Its own
code uses std and follows the panic/indexing and unsafe contracts. The initial
backend uses the approved Rustls/AWS-LC category. AGENTS.md and the shared
host/sandbox gate admit only the exact named
manifests, locks, root Cargo configuration and selected normal/build features in
`builder/src/crypto_policy.rs`. Other roster closures remain std-only.
Changing any pinned input requires an explicit policy update. Cargo config
files above the repository and either crate's automatic build.rs are refused.
`DEPENDENCIES.md` records the complete locked inventory and active subset.

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
backend in source, with no feature, environment or runtime backend selector.

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

The four fixed errors are Capacity for insufficient output, Invalid for
malformed/unsupported/inconsistent key input, Entropy for recoverable random
source failure and Crypto for other recoverable provider operation failures.
They expose no provider diagnostics, nested provider error, secret bytes or
input-dependent formatted message. td-mta re-exports the shared traits and
maps these errors into its own port error enum. The shared crate never imports
that enum. TLS-specific errors and interfaces are frozen by M07 before their
consumers are implemented; they follow the same ownership and redaction rules.

## Backend and TLS implementation

### Streaming SHA-256

`Sha256::try_new` creates one opaque digest operation. Initialization is cold
and may allocate provider state; failure returns `Error::Crypto`. `Digest`
updates stream directly into that state without retaining whole input chunks;
native state retains up to 63 bytes of a partial block. The future concrete
`Crypto` factory delegates to the same `Sha256::try_new` constructor.
Checked total input length cannot exceed `u64::MAX / 8` bytes, the SHA-256
bit-length limit. Length refusal or a provider update failure retires the
state; later updates and finish return `Error::Crypto`. Only a successful,
consuming finish returns a 32-byte digest. No clone or reset is exposed.

The pinned provider's public Context constructor, update and finish use
unwrap/expect internally; their fallible helpers are private. A narrow
`catch_unwind` boundary owns the context across each call. On unwind it drops
the context and returns the fixed error; it never resumes a partial context.
No `AssertUnwindSafe`, global hook change, vendor patch or second backend is
introduced. The pinned digest cleanup calls native `EVP_MD_CTX_cleanup`
without a Rust panic path. Algorithm selection is fixed to SHA-256; the
audited Rust failure payloads are fixed messages and `Unspecified`, not input
bytes. No provider error or panic payload crosses the public API.

This boundary requires Rust unwinding throughout the target graph, including
the final executable. The crate refuses its own compilation with `panic=abort`;
that check alone cannot detect linking an unwind-built rlib into a separately
compiled aborting executable. The supported portable Cargo build compiles the
whole graph with the default unwind strategy. Every future target recipe or
consumer build must preserve that strategy through final linking; a final-only
abort override is unsupported. Panic hooks still execute, and an aborting or
panicking hook is outside the boundary. Native aborts, OOM and unwinding failures are not contained.
Returned failure tests inject Rust unwinds at the same private entry points;
they do not establish native allocation-failure or entropy-failure recovery.

Successful native finalization explicitly cleanses digest state. Every cleanup
path, including early/error drop, also calls `EVP_MD_CTX_cleanup` and then
`OPENSSL_free`. The pinned default allocator cleanses the allocation, including
its size prefix, before freeing it. No allocator override is installed; any
future override must preserve this responsibility and repeat qualification.
This source audit does not prove erasure of provider stack temporaries or
compiler/caller copies. Never finalize a failed context merely to clear it.

Known-answer tests reuse the four existing engine SHA-256 fixture values and
exercise empty inputs and fragmented updates around block boundaries. The
portable harness runs those cases, retirement checks and unwind injection in
its isolated runtime. This is functional and API-confinement evidence; M07
still owes native allocation, worker-stack and whole-process qualification
before service use. Successful construction allocates native digest state,
and the public provider API exposes no reset operation. Constructing one per
message cannot satisfy td-mta's no-allocation hot-path contract by calling it
admission work. Cold configuration/startup work can use this facade; reusable
state or a separately qualified implementation is required before hot-path
hashing is enabled. The remaining Crypto operations and concrete factory
are pending.

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
AWS-LC version pair for direct operations and TLS. No second backend enters the
shipping graph. Disable unused features and all undeclared build fallbacks.
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

M07 records accepted sets and configured preference orders for TLS versions,
cipher suites, key-exchange groups/PQ policy, and separate certificate and
handshake signature schemes. Record accepted PEM labels, DER forms, key
curves and RSA sizes; reject unsupported policy inputs. Provider defaults
cannot silently expand that baseline. Dependency or backend changes compare
against it. Persist standard keys/certificates/digests, never provider contexts
or backend IDs. Retain non-secret golden key fixtures, including generated,
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
existing engine SHA/Ed25519 and td-secret P-256 implementations and their audit
evidence before reusing any code. Reuse confers no unproven side-channel claim
and does not migrate those consumers into this external-dependency crate.
Cross-component primitive consolidation would require a separate design;
it is outside F04. td-mta/IMPLEMENTATION.md F04 owns mail integration staging
and cutover. Each primitive needs
independent cryptographic and exact-artifact side-channel review, including
compiler/flags/target, nonce generation and entropy/optimization-barrier choices.
Passing functional tests alone is insufficient. Test candidates stay outside
the shipping backend until an atomic qualified replacement removes obsolete
AWS-LC dependencies/build inputs. Do not prebuild unused primitive interfaces.
