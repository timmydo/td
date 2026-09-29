# Cryptography and TLS boundary

## Status and ownership

This crate owns td's provider-independent cryptographic API. The compiling
M03a surface contains `Error`, `Entropy`, `Digest` and `Crypto`, extracted
from td-mta's existing ports. It currently implements no cryptographic
algorithm, entropy source or TLS session and has no external dependencies.
Mocks and interface tests are not cryptographic conformance evidence.

The target dependency graph is:

```text
td-mta (library and eventual executable)
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
backend uses the approved Rustls/AWS-LC category. Before adding that closure,
M03b must atomically amend AGENTS.md and the shared host/sandbox gate logic:
permit the named crypto crate's pinned closure and the mail crate's exact
transitive closure, preserving restrictions on every other roster crate.
The present std-only locks need no exception.

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
`Crypto` owns SHA-256 construction, fixed 32-byte constant-time comparison,
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

M03b pins exact versions, features, licenses, native build inputs and roots.
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

M03b's public-API confinement must cover nested exports, renamed aliases,
public signatures and associated types, with mutations for each escape. The
M03a doctests check only two named root exports and are insufficient evidence
for that backend boundary.

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
