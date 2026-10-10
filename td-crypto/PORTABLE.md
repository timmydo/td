# Portable build inputs

## Status

M03b2a implements header preparation; M03b2b prepares the remaining Rust and
GNU tool inputs. M03b2c supplies isolated compilation and static artifact
qualification. M03b2d1 adds API confinement; M03b2d2 adds local TLS smoke.
This is a host build path for the
standalone td-mta executable; it does not grant the source-bootstrap provenance
of td's target image graph. Nothing prepared here is automatically admitted
as a target recipe input.

## x86-64 musl headers

The header source is musl 1.2.5, matching the version named by Rust 1.96.0's
`src/ci/docker/scripts/musl-toolchain.sh`. Rust's target standard library owns
the linked libc and its upstream patches. This command generates headers;
it does not build or replace libc, and does not claim that an unpatched musl
1.2.5 runtime is suitable for deployment. The complete Rust target archive
pin is below; runtime qualification belongs to M03b2c/M03b2d.

| Input | Pin |
| --- | --- |
| Upstream archive | `https://musl.libc.org/releases/musl-1.2.5.tar.gz` |
| Archive bytes | `1080786` |
| SHA-256 | `a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4` |
| Architecture | `x86_64` |
| Installed header count | `218` |
| Header tree SHA-256 | `a673b15579a83881c2d9aa61a65ce26dd62557d16c497e5a04e4e6a8bb6d601b` |

Prepare a previously fetched archive offline:

```text
td-builder gate-crates crypto-musl-headers --archive /path/to/musl-1.2.5.tar.gz
```

The command prints the prepared directory under the worktree's
`.td-build-cache/crypto-musl-x86_64-1.2.5-<output-digest>`. The archive argument
must name a regular file, not a symlink. It verifies a bounded in-memory copy
of the archive and passes those same bytes to td's existing gzip/tar reader. An
oversized archive fails the bounded read before hashing. It
does not fetch, run `configure`, execute `make`/`sed`, or search host headers.
Its std-only generator follows musl's installed-header rules: public include
files, architecture bits overriding generic bits, guarded alltypes
declarations, and syscall aliases. This is a version-specific generator;
changing the archive or architecture requires reviewing the rules and output.

The tree digest covers each relative header path in ascending byte order,
then NUL, the decimal byte length, NUL, and the file bytes, concatenated into
SHA-256. It was derived from the complete upstream `make install-headers`
output, independently of td's generator. Every invocation checks the generated
tree against it, including the header count. This catches wrong overrides,
declaration transformations, omitted files and extra files.

Output contains `include/`, the upstream `COPYRIGHT`, and a `SOURCE` receipt
naming the URL, archive hash and header-only scope. Musl is predominantly
MIT-licensed; retain the complete upstream notice for its additional notices.
A fresh output is staged privately and published as one directory. The suffix
hashes all output file paths and bytes with the same encoding as the header
digest, so a changed receipt or license selects a new cache entry. Existing
output is re-read and compared with freshly verified source bytes, including
the license and receipt. Modified, missing, added or symlink files fail;
the command does not silently repair them. A concurrent identical publication
may be reused. Empty extra directories carry no build input and are ignored.
The host cache is caller-owned, not a security boundary against another process
with the same uid. A later isolated compilation must bind its verified input
read-only and exclude undeclared paths and ambient flags.
On a cache mismatch, remove only the named cache entry and retry. This cache
is reconstructible and publication is not fsynced. A hard-killed preparer may
leave a private `crypto-headers-<pid>-<attempt>` scratch directory; remove it
only after confirming that preparer is no longer running.

Other architecture header rules are not yet implemented. This x86-64 command
does not prevent a separately pinned and tested ARM path later.

## Rust host kit and target standard library

The portable path uses the upstream Rust 1.96.0 GNU host compiler, Cargo,
GNU host standard library and musl target standard library. The compiler and
its target library must be the matching upstream build: td's source-built
Rust identifies itself differently and rejects the upstream target library
with E0514. This host-only exception does not admit any downloaded Rust
component into td's source-built distribution. No rustup state is changed.

All four archives are under the upstream prefix
`https://static.rust-lang.org/dist/2026-05-28/`:

| Archive (`.tar.xz`) | Bytes | SHA-256 |
| --- | ---: | --- |
| `rustc-1.96.0-x86_64-unknown-linux-gnu` | 80188348 | `7d7fa1d0cfb0fab71a956bb78f41107202c17f30ab56c45288e869a37fd9633d` |
| `cargo-1.96.0-x86_64-unknown-linux-gnu` | 11154668 | `dee75c3c8f9f600ad75bc0c93249e767d3047845a4dd668327ce43ab039ba266` |
| `rust-std-1.96.0-x86_64-unknown-linux-gnu` | 29740704 | `c09c7c646248f14f473f5f7a029af15ee57c3a9f9bc93dfa72d9621938586b82` |
| `rust-std-1.96.0-x86_64-unknown-linux-musl` | 38833276 | `4db24564076c243b377585bec0b938388638301ea5003d6245c4cbf9d86b11d0` |

These hashes are from the Rust 1.96.0 upstream distribution manifest. Prepare
those four files and the musl header archive in one directory, then run:

```text
td-builder gate-crates crypto-portable-prepare --archives /path/to/archives
```

Archive paths must be regular files, not symlinks, and remain unchanged during
preparation. The caller owns the input and cache trees; concurrent mutation
by another same-uid process is outside this boundary. The command verifies size
and SHA-256 while copying each archive into private scratch space, then
extracts only that verified copy using td's native tar/xz reader. It overlays
the known component directories, refusing file clashes and retaining executable
bits, upstream installed notices and separate root notices for each package, including Cargo's
`LICENSE-THIRD-PARTY`. Neither `install.sh` nor other upstream install logic
runs. `PORTABLE-INPUTS` inside the kit records URLs, lengths and hashes.

The kit is published under `.td-build-cache/crypto-rust-1.96.0-<NAR-sha256>`.
The build additionally requires NAR SHA-256
`dce3040c995d6572fd47df15fded5d5e3ea6c87455b5daef82f4dea2503942b1`.
This pins the reconstructed layout and notices as well as archive bytes.
Update this build pin only with a reviewed reconstruction/input change and
repeat the isolated artifact qualification.
Every invocation reconstructs the expected kit from authenticated archives;
reuse compares the complete tree's NAR hash, including links, executable bits
and notices. A changed or symlink cache root fails. No persisted digest alone
authenticates a mutable tree. This prepares binaries and libraries as data;
preparation alone does not execute them or prove their runtime closure.

## Declared GNU tool and host runtime inputs

The preparer rebuilds or reuses a source-fingerprinted td-recipe-eval and asks
it to realize these named outputs from the current checkout's recipes:

| Graph | Output | Intended isolated role |
| --- | --- | --- |
| `gcc-x86-64-self` | `gcc-x86-64-self` | GCC 14.3.0 C compiler, `/cc` |
| `gcc-x86-64-self` | `binutils-x86-64-self` | binutils 2.44, `/binutils` |
| `gcc-x86-64-self` | `glibc-x86-64` | glibc 2.41 host loader/libraries, `/lib64` |
| `gcc-x86-64-self` | `gcc-x86-64-stage2` | host libgcc_s, `/gcc-runtime` |
| `zlib-x86-64` | `zlib-x86-64` | shared zlib 1.3.1 host runtime, `/zlib` |

The recipe graph and its fixed-output source declarations are the authority;
no caller-supplied compiler path or guessed store hash selects a tool. The
preparer does not use `TD_RECIPE_EVAL`. Realization may provision declared
sources; a cold source bootstrap can take hours. The command is not a claim
of completely offline provisioning. The later compilation must be offline.
Direct preparation has no wall-clock deadline; its child has the existing
parent-death handling. Logs retain at most 30 input lines for failure diagnostics, each truncated
to 64 KiB before lossy decoding and marked when truncated. Only output/work
records require complete UTF-8 lines. A failed realization reports its exit
status and tail; an unreadable log still reports the exit status. Logs are
transient and reused for each graph, then removed on completion or error.

The evaluator's work record and output basenames select complete trees under
its durable `build-cache/store`. Reclaimable evaluator staging trees are not
copy authorities: a failed scratch cleanup could leave a truncated tree there.
Durable cache reclamation renames whole trees away before deleting them.
Each selected output is hashed and compared with any retained cache entry.
A matching retained entry avoids another copy. A cache miss is copied into
private scratch, compared with the source hash, and published atomically.
Disappearing inputs or changed bytes fail and require a retry. This uses the
recipe store's existing trust model; the preparer records its bytes and does
not independently re-prove a warm build's source provenance. Cache reuse
requires a fresh durable-output comparison.
Role directories are resolved within the retained output; a changed layout or
escaping directory is refused. Internal tool links are retained as data, not
followed to import host files. The isolated build below resolves these links only inside its declared mounts.

Success prints `portable-rust`, `portable-musl-headers`, and pairs of `native`
recipe identity/NAR records and `portable-native-bind` role/directory records.
This is preparation output, not an installed artifact manifest. No tool flags,
API confinement, static-linking result or service capability is established by
this command alone.

All these caches are reconstructible and caller-owned. On mismatch remove
only the named cache entry and retry; there is no automatic repair or fsync
claim. A hard kill may leave `crypto-portable-<pid>-<attempt>` scratch behind;
remove it only after confirming that process has ended. The build host needs
space for archives, extraction, reconstructed kit and retained native outputs.
Each extracted Rust component and its private archive are removed after
installation. Superseded content-addressed kits and each worktree's native
copies remain until explicitly removed. Remove an old `crypto-*` cache entry
only when no preparation or build uses it and it is no longer needed; a later
preparation reconstructs it. The final evaluator scratch tree also remains
until the evaluator's ordinary later-run cleanup and may occupy several GiB;
this command does not sweep shared evaluator scratch. Service memory budgets
do not describe build-time resource use. ARM host and target support requires
a separate manifest/layout qualification.

## Isolated compilation and artifact

```text
td-builder gate-crates crypto-portable-build --archives /path/to/archives
```

This explicit qualification command first performs the same input preparation
and provisions the locked registry archives through td-feed. Preparation can
fetch declared fixed-output sources. Compilation runs without network access.
The ordinary Cargo preflight still qualifies the host build; it does not
silently claim to have run this separately provisioned portable command.

The driver stages manifests, locks, `src/` and optional `tests/` for
td-crypto, td-header, td-json, td-mime, td-nfc and td-mta, plus the three test-only
oracle files listed below. It refuses symlinks and special files. It
rechecks staged manifest/lock pins, reconstructs the verified vendor tree,
and mounts these inputs read-only. It uses the existing
source-fingerprinted static td-builder helper for namespace entry, host
linking and failing fallback decoys. This helper is host control plane,
not part of the installed binary. Build/output and Cargo home are private;
no previous Cargo target directory is reused. Caller-owned source/cache
trees must not be concurrently modified. No hostile same-uid writer
boundary is claimed.

The namespace has only these mounts plus the Rust kit, musl headers and five
native roles listed above. It has private `/tmp`, minimal `/dev`, private `/proc`
and no host `/usr`, `/bin`, OpenSSL installation or Cargo configuration. Cargo's
environment is cleared and reconstructed. The inner helper records each actual
Cargo command's arguments, working directory and explicit environment in
`BUILD-INPUTS`; these include frozen/offline source replacement, two jobs, epoch 0,
explicit Rust/C/ar/linker paths, all five AWS-LC source-build controls and
the fixed bundled-SQLite selection and compile-limit controls.
CMake and pkg-config point to a failing td-builder applet which writes a marker;
OpenSSL points to a nonexistent directory. The positive build refuses any marker.
A missing compiler, header, archive or library is an error, with no ambient
substitute. There is no Zig dependency.

Rust links the target through its bundled rust-lld, static musl target std and
`+crt-static`, with `target-cpu=x86-64`. GCC uses `-nostdinc`, only the declared
musl/GCC builtin includes and binutils, and `-march=x86-64 -mtune=generic`.
Compiled Rust and C request frame pointers and level-1 debug information;
release/bench profile stripping is explicitly disabled, and the artifact check
requires structurally valid `.debug_line` data. Source/vendor/output paths map
to `/td-build`, `/td-cargo/vendor` and
`/td-build-root`. Upstream prebuilt std/libc and hand-written provider assembly
remain profiling boundaries. This host portable artifact is not the target
image's whole-closure profiling qualification. Debug information remains in
the executables; distribution debug-companion integration is not claimed.

Cargo's selected normal/build graphs must match admission. Its artifact records
must select exactly one expected binary/test profile. All eight results must be
x86-64 static PIEs with an executable entry point and no ELF interpreter,
DT_NEEDED or runtime search path. A second fresh namespace mounts only the
result and static test supervisor, then runs the installed name's version command
and each SHA-256, mail-format, PEM/identity/trust, entropy, provider-construction,
TLS, mail clock/TCP/TLS transport and configuration-stack smoke case in its own
process.
It has no compiler, root-data file, loader or library mounts. Each runtime command has a
30-second deadline; each Cargo command has a 20-minute deadline. Parsed Cargo
stdout is limited to 8 MiB (graphs to 256 KiB); each JSON record is limited to
256 KiB and 64 nesting levels. Logs on disk are temporary, not a streaming
output quota. M07 owns native/TLS allocation and entropy-failure qualification;
a Result wrapper cannot contain provider aborts.

After the compile namespace exits and its descendants are reaped, the host
requires an exact regular-file output inventory: eight binaries and the inner
command record. It rejects output directory/file symlinks and additional files,
then copies these checked inputs into a fresh private directory outside the
compiler's writable mount. Only this directory receives host-written metadata
and notices, and only it is bound into the runtime fixture and published.
The internal commands require the host-sandbox marker and expected input paths
before writing. This guards accidental direct invocation, not callers forging
their environment; namespace entry remains the isolation boundary.

Success prints `.td-build-cache/crypto-artifact-<NAR-sha256>`, containing:

- `td-mta-rss-probe`: separate unwrapped process RSS observations, with no
  service use or inferred whole-service/transient-peak bound.
- `td-mta-native-allocation-probe`: separate libc forwarding diagnostic with
  explicit registry/counter/per-thread-flag storage evidence and no service use.
- `td-mta-rust-allocation-probe`: dedicated System-forwarding allocation
  qualification process; never an installed executable or service dependency.
- `td-mta`: the installed executable name, currently only `--version`/`--help`;
  all service arguments fail. It does not yet serve mail.
- `td-crypto-smoke`: separate qualification test executable, not a service
  dependency. It exercises native code that the current packaging entry point
  does not yet retain through a service caller.
- `td-mta-config-smoke`: integration test linked to the production td-mta
  library for the bounded configuration stack checks in td-mta/CONFIG.md.
  Generic loader/materializer/writer functions compile in the test crate;
  this does not qualify a future installed service caller, even with the
  same reader type. It is not a service dependency. The same release Cargo
  graph builds it with `--test config_stack --no-run`; selection requires
  a test target and test profile. Runtime selects each ignored case by its
  exact name and requires one passed test plus one positive bounded mapping
  measurement. `portable_loader_stack` retains its 176 KiB ceiling and
  `config_stack_mapping_bytes` label; `portable_materialized_stack` checks
  256 KiB and emits `config_materialized_stack_mapping_bytes`. Measurements
  are relayed to the build log, not retained in `BUILD-INPUTS`.
- `td-mta-format-smoke`: `--test format_rows --no-run` integration executable
  for the two exact provider hash-coverage cases below. It has the same
  test-target/profile, static ELF and isolated-runtime checks as the other
  qualification binaries; it is not an installed service dependency.
- `td-mta-transport-smoke`: the `--lib --no-run` td-mta test executable,
  selected by its `td_mta` library target and test profile. Sixty-five
  exact SMTP/policy/generation/gateway/admission/clock/TCP/TLS cases execute
  individually under the existing deadline and positive-one-test verdict
  requirement. Six inbound STARTTLS cases cover reserved-session ownership,
  framed bare commands and tails, short writes/complete flush, deadlines and
  refusal cleanup, direct TCP/TLS handoff and private gateway STARTTLS with
  TLS 1.2/1.3 pin acceptance/refusal. The private peer waits for the exact
  plaintext 220 before starting TLS. These start at the STARTTLS command
  boundary; complete greeting/EHLO/transaction/reset semantics remain pending.
  One ignored release-only stack case reruns twenty-five policy/transport
  scenarios sequentially on a requested 240 KiB worker. The bounded smaps
  reader requires a guarded, non-growing mapping within 256 KiB and emits
  `tls_policy_transport_stack_mapping_bytes`; the portable driver requires
  exactly one bounded decimal observation. This qualifies only those exercised
  test-compiled paths. It excludes the separate gateway peer's stack and does
  not establish native heap, maximum-input/CPU-path coverage or service RSS.
  The same combined scenario set runs in an ordinary host test for fixture
  drift only; host execution provides no target stack bound.
  Native C/assembly stack probing is not qualified: a native frame could skip
  the guard. Successful execution and an observed mapping do not prove every
  native overflow would fault. RESOURCES.md retains that admission obligation.
  Five outbound additions cover complete EHLO offers, malformed extension
  skipping, fragmented/multiline 220, buffered-tail refusal, role/scratch/time
  admission, cancellation and both upgrade owners through verified local TLS.
  They supply the EHLO boundary and do not implement the complete relay driver.
  Eight earlier SMTP control-framing cases cover fragmented strict CRLF,
  line/reply ceilings, multiline code agreement, exact tails, bare final codes,
  malformed syntax and EHLO greeting/extension separation. They also cover
  complete TLS policy compilation and HTTPS name subsets, reader
  caps/refusals, client-only startup/expiry admission,
  strict client trust and complete-table replacement within the same
  generation budget, generation-bound IDs, queued/native session ownership,
  exact buffer recovery and gateway policy/binding comparison; local
  compiled-policy peers verify relay trust, name routing and mandatory
  client authentication. Retained TCP handoff cases cover plaintext tails,
  exact buffer recovery, fixed/tightened deadlines, encrypted
  delivery/close, generation and handshake capacity, clock failure and
  changed-policy abort. Two gateway process cases use the private td-crypto
  test peer for TLS 1.2/1.3 current/next pin acceptance and verified-leaf
  wrong-pin/actual-peer refusal. Runtime sets the peer executable to
  /artifacts/td-crypto-smoke; the mail test accepts only its own ephemeral
  loopback connection, and the child verifies the server certificate/name.
  No test client-auth API enters the public facade. They also cover
  generation saturation before construction, stale/foreign publication,
  detached worker construction/retention, boxed payload handoff on a small
  requested stack, final payload drop order and construction/release races;
  canonical gateway policy equivalence/change, pin/CIDR and material
  refusals, mandatory client authentication against a positive
  no-client-auth control, handshake count limits, concurrent reservation and
  release, worker returns and refusal cleanup, clock conversion, bounded
  TCP/half-close/failure, TLS 1.3 duplex progress and framing, backpressure,
  publication deadlines, closure and truncation, and buffer recovery after
  success or constructor refusal, with both borrowed and owned reservations,
  including owned connection transfer through a worker thread. Socket
  fixtures bind only ephemeral IPv4 loopback ports; certificates are
  generated locally through the public crypto facade. No provider, CA or
  deployment server is contacted. This qualifies those adapter behaviors on
  musl, not full service admission, native allocation/stack/RSS, or
  complete SMTP/STARTTLS protocol integration.
- `BUILD-INPUTS`: staged source, vendor, reconstructed Rust kit, headers and
  helper NAR hashes, native recipe identities/NARs, target and actual inner
  Cargo argument/environment records.
- `notices/`: upstream LICENSE/COPYING/COPYRIGHT/NOTICE/AUTHORS files, including nested
  provider attribution and Rust's bundled library notices. Vendor notices cover
  the entire locked inventory, including inactive and build-only packages;
  every vendored package must contribute at least one notice. Their presence
  does not claim that all packages are linked.

The artifact hash covers binaries, receipts and notices. Each invocation rebuilds
from a fresh target directory; an identical result reuses an existing verified
artifact. Changed or symlink cache entries fail instead of being repaired.
Concurrent identical publication is allowed. No fsync durability is promised
for reconstructible build caches. Private `crypto-build-<pid>-<attempt>` trees
are removed on normal completion/error; after a hard kill remove one only after
confirming its process ended. Retained artifacts follow the same explicit cache
cleanup rule as prepared inputs.

## Private gateway process peer

Host mail tests run through `gate-crates crypto-cargo test --manifest-path
td-mta/Cargo.toml`. That wrapper builds td-crypto's private library-test peer
from the same reviewed offline closure, selects its exact Cargo JSON test
artifact within the configured target directory and injects its absolute path.
Ambient TD_MTA_TEST_TLS_PEER cannot select a substitute. The peer entry point
is ignored by ordinary crypto tests and invoked only with synthetic fixture
material and a loopback socket. The parent bounds elapsed time and
reaps its owned child; the child bounds individual socket operations. The portable runner supplies the same
peer from the already qualified static test executable without toolchain mounts.
These are test orchestration processes; they do not add a service worker or
claim runtime allocation/stack/RSS qualification.

## Streaming digest qualification

The runtime runs the owned SHA-256 facade's four existing known-answer
fixtures, including fragmented updates around block boundaries and a million
`a` bytes. Differential cases compare against the admitted native SHA-256
implementation at every byte length through 257 and selected larger block
boundaries through 4097, with varied chunks and every split through 257.
Separate cases verify terminal refusal, retained-state clearing, bounded inline
storage and redacted Debug. The length limit uses a synthetic counter near its
ceiling; synthetic differences exercise upper padding-length bytes without
hashing exabytes.

Direct digests have no provider unwind path, heap allocation or native crypto
calls. Source and exact-artifact control/address and call-graph review qualify
that narrow primitive; retain compiler/flags/target and artifact identity in the landing
record. This is not a guarantee for arbitrary compiler changes, other CPU
paths, secure erasure of all copies, or whole-service resource use. The native
adapters still require whole-graph `panic=unwind`; their synthetic failures do
not simulate native abort/OOM or entropy failure. DESIGN.md owns those limits.

## Entropy qualification

Three exact cases in `td-crypto-smoke` exercise local bounded random fills,
nonempty constructor initialization, and synthetic returned errors after a
partial write. They verify that failed construction yields no handle and a
returned fill error clears the whole caller slice while preserving guards.
The local sample comparison catches no-op/constant-output wiring; it is not
an entropy-quality test. The host compile-fail doctests separately pin the handle's
Send and Sync refusals. No new portable executable is added.

These cases do not induce native RNG failure or prove recovery from native
abort/OOM or bounded native wait time. Process-wide/per-thread state, shared
locks, direct native stderr, warm-up, cleanup and remaining allocation/stack/RSS
qualification follow DESIGN.md. No service is enabled by this probe.

## Crypto factory and P-256 qualification

The existing crypto smoke executable also runs the factory/comparison, accepted
PKCS#8 forms/public-point answer, malformed/inconsistent-key refusal,
generated-key/signature verification, output/retirement and shared-key cases.
Its independent test-only oracles reuse exactly engine/src/sha256.rs,
td-fido/src/fido_p256.rs and td-fido/tests/p256_vectors.txt. Stage these
regular files at their original relative paths and include their bytes in the
source digest. Missing files, symlinks and non-directory ancestors refuse the
build. No other engine/td-fido file is staged and no Cargo dependency is
added. Neither oracle is compiled into the installed td-mta executable.

The runtime separately runs the oracle's four SHA-256 known-answer cases and
all seven P-256 arithmetic, point, range and signature-vector cases, requiring
one exact passing result for each. Functional comparison results do not prove
constant-time lowering. Key/signing returned-error and unwind injection do not
induce native RNG failure; native allocation, blocking, stack/RSS and timing
qualification remain M07 obligations. DESIGN.md owns the accepted key formats,
secret lifecycle, output/retirement contract and native failure limits.

## Mail-format digest qualification

`td-mta-format-smoke` is the separate `format_rows` integration executable.
Its two selected provider tests call only the td-crypto facade. They compare
literal SHA-256 for blob `abc` and both independently calculated import
snapshot preimages. Updates use 1/31/64/65-byte chunks; changed first/middle/last
preimage bytes produce different digests. Removed custom containers have no
portable selector or hash-coverage claim. No fixture is regenerated by the
code under test.

The installed executable remains separate. These tests establish literal hash
coverage and facade integration, not production container decoding, filesystem
trust, publication, recovery, allocation bounds or service operation.

## Local TLS smoke

The test-only backend module generates an ephemeral P-256 root and localhost
leaf with the admitted signing backend. A small fixture DER encoder builds
certificates normally valid from 2025-01-01 to 2035-01-01; the client-expiry
case ends its leaf's validity at 2026-01-01. No external certificate,
private key, server, OpenSSL command or new dependency is needed. Both peers
receive an explicit provider and fixed clock. The client trusts only this
fixture's CA, no roots for the untrusted-chain case, or a same-named CA
with a different key for the bad-signature case. The process-global
provider stays unset.

Sixteen baseline cases run separately in the clean runtime; two additional
algorithm-policy cases are specified below:

- TLS 1.2 and TLS 1.3 each negotiate the requested version, exchange binary
  plaintext in both directions and observe orderly closure on both peers.
- TLS 1.3 rejects a wrong DNS name, an untrusted chain and an expired leaf,
  checking the specific backend certificate error in each case.
- A server rejects an invalid TLS content type as a malformed record.
- TLS 1.3 rejects the wrong CA key with a certificate-signature error and
  rejects a flipped authentication-tag byte with a decryption error.
- TLS 1.2 and TLS 1.3 each require a trusted client certificate, complete
  a full handshake, expose the exact verified client leaf and transfer data.
  Two connections reuse each of three configuration pairs: both sides disable
  resumption, only the remote client enables it, and only the remote server
  enables it. Every connection must perform a full handshake, independently
  qualifying the tested server and client settings. Early data stays off.
  The mutual setup also refuses a wrong server name in each version.
- Five cases run both versions and require specific errors for a missing
  client certificate, unknown issuer, expired client leaf, server-only usage
  and bad issuer signature. Client expiry alone changes in that case; the
  server certificate and verification clock remain valid. The error pins
  the verification and expiry timestamps. Refused clients leave no peer chain.
- One case runs both versions and refuses a client certificate/private-key
  mismatch during configuration, before creating connections.

The mutual-authentication fixtures generate distinct client and server keys
and use unique leaf serials under their generated CA. No client-chain result
is gateway authorization; the mail adapter must apply its peer policy after
handshake completion. These are private backend tests, not service adapters.

The baseline round trips assert X25519 key exchange and AES-256-GCM/SHA-384 suites
for both versions. The fixture certificate and handshake signatures use
ECDSA P-256/SHA-256. The algorithm-policy cases below cover the additional
classical groups and suites. RSA, P-384, P-521 and Ed25519 signing schemes
remain inventory-only; alternative CPU/assembly paths are not exercised.
This check covers the CPU paths selected on the test host.

Each drive uses a 32 KiB caller buffer and permits at most 256 KiB wire
traffic and 64 bidirectional turns. Each round-trip case calls drive five
times; each mutual-authentication positive case uses six successful
connections with two drives each, plus one wrong-name handshake refusal.
The five client-certificate refusal cases attempt
one handshake per version; the key-mismatch case creates no connection.
Each connection's Rustls application-data send-buffer limit is
32 KiB; this does not constrain queued handshake or alert messages.
Zero progress and exhausted budgets fail the test;
each case also has the supervisor's 30-second process deadline. A failed
runtime case prints its captured stdout when it fits the 64 KiB log limit;
negative tests include the actual backend error in assertion diagnostics.
Fixture/key
construction may allocate. These work limits do not bound provider memory,
CPU time inside a call or stack, and are not the production transport API.
M07 still owns production TLS policy, adapters, secret handling, allocation
and fatal-failure qualification. This smoke proves local interoperability
within the pinned backend, not independent cryptographic conformance.

## Public API qualification

Before publication, the portable command compiles the production td-crypto
library again in a separate diagnostic target directory, selecting `-p td-crypto`
through td-mta's Cargo manifest, lock and release profile, with the same target,
native controls and Rust target flags as the shipping binary. This ordinary
stable compilation forbids `unexpected_cfgs` and declares
`--check-cfg=cfg(doc,debug_assertions,values())`. Every reachable use of either
condition, including cfg_attr, cfg! and expanded macros, is an error. Source
lint allowances cannot override the command's forbid. The later diagnostic
commands repeat this guard. These conditional branches are unsupported:
rustdoc adds `doc` and keeps `debug_assertions` enabled despite release flags.

The exact manifest pins admit neither custom build scripts nor Cargo check-cfg
declarations that could merge additional allowed conditions into this guard.
Those are forbidden in future pin updates too; changing this rule requires
requalifying the guard. Mutation tests exercise the pin refusals.

A second compiler pass marks all three direct backend dependencies private
using `--extern priv:NAME` and forbids `exported_private_dependencies`. This
also catches transitive foreign types and methods/associated types involving
`dyn` local traits that pinned Rust 1.96 rustdoc omits from its JSON. The compiler
pass is mandatory; the graph alone is insufficient. Both the privacy pass and
rustdoc use crate-scoped `RUSTC_BOOTSTRAP=td_crypto` for diagnostic options only.
The shipping binaries have already been compiled and copied; their build uses
no unstable options. No nightly toolchain, Rust parser dependency or source-text
re-expansion is used. Cargo supplies the crate environment and dependency
resolution. Actual commands/environment and graph/fixture counts enter
BUILD-INPUTS.

Pinned rustdoc produces schema-57 JSON including private and hidden items. The
std-only walker follows public modules/re-exports, aliases, fields, variants,
functions, methods, generic bounds, traits and associated types. It follows
trait implementations attached to reachable local types, including private
traits. This deliberately rejects private-trait backend types on public
wrappers; private backend fields remain allowed. Every referenced type must
belong to td-crypto or std/core/alloc. Missing IDs, unknown public item kinds,
exported macros, changed schema or incomplete output fail.

The JSON target triple and enabled CPU features must match baseline x86-64
musl. Schema 57 omits `crt-static` from its CPU-feature list, so a generated
Cargo fixture using the same command constructor proves static musl/non-test
conditional exports survive. Two more Cargo probes prove the actual compiler
invocation enforces the cfg guard and private-dependency rule; the latter
first compiles the leaking program normally, then requires the privacy error.
Target conditions remain supported. Other targets/features need qualification.
This checks the public type boundary, not semantic behavior of method bodies.

The graph conservatively refuses imported blanket traits that expose foreign
traits on public types. Facade handles must be defined at module scope:
rustdoc may omit function-local definitions reached through returned impl
Trait, and unresolved IDs fail rather than being ignored.

JSON is limited to 16 MiB, 128 nesting levels and 65,536 entries per ID table
and reachable graph. Each production diagnostic Cargo command has a 20-minute
deadline and 8 MiB stdout limit. Generated fixture compiler commands have
30-second deadlines and 256 KiB stdout/stderr parsing limits; the small Cargo
probes have 120-second deadlines and 8 MiB stdout limits. Temporary disk logs
are not streaming quotas. Negative cases require normal exit code 1 for rustc
or 101 for Cargo and the expected structured compiler diagnostic. A timeout,
signal, unrelated failure or exceeded bound refuses publication.

Each portable build compiles owned fake backend/leaf crates and valid Rust
consumers. Positive fixtures retain private provider state and owned associated
types. Negative fixtures cover nested/renamed/glob/hidden exports, aliases,
signatures, bounds, methods, tuple/enum/union fields, constants, associated
types, dyn-trait methods/operator arguments, transitive types, exported macros,
musl/static conditions and direct/local/external-macro doc conditions.
The doc/release-condition fixtures first compile without the guard, then
require the cfg error. Foreign-type cases require the private-dependency error;
except for the omitted dyn impls, their valid JSON must also reject the foreign
reference. Exported macros are rejected by the graph.

## PEM syntax qualification

The crypto smoke artifact also runs the bounded PEM fixtures: canonical
base64 and unchanged error output, independent certificate decoding,
complete-envelope validation and short-buffer retry, exact byte/count/DER
limits, and generated P-256 PEM loading/refusals. These operate on in-memory
fixtures and use the already admitted PEM reader as an independent decoding
oracle. No certificate trust, identity publication or service resource
qualification is implied.

## Local identity qualification

The same crypto artifact runs local identity admission, canonical metadata
and certificate algorithm inventory fixtures. Generated local chains cover
P-256 and RSA issuers, names/time/key mismatch, usage, issuer/path/name
constraints, exact name/extension/depth limits, malformed DER and metadata,
and synthetic Rust construction unwinds. Existing TLS fixtures share their
certificate generator with these tests. Every case remains in-memory and
contacts no CA or deployment. This does not qualify trust-store selection,
TLS sessions or native allocation/stack/RSS admission.

## Trust-store qualification

Five additional cases cover explicit private root replacement with disjoint
local CAs, server/client certificate usage, fixed public-root inventory,
input ownership and caps, duplicate/malformed bundle refusal, supported
anchor key families, the restricted private-CA subset and construction
unwind. Anchor dates and self-signatures deliberately are not peer identity
checks. The fixtures qualify material admission and backend path use; they
do not qualify configuration-role enforcement, TLS Finished, gateway
permission or the service's resource budget. No test contacts a public server.

## Explicit algorithm policy qualification

The portable artifact runs the exact suite/group/signature inventory and
mapping-drift fixtures, plus a generated ML-DSA-signed leaf accepted by the
native baseline and refused by the selected certificate policy. Backend
TLS fixtures use the selected provider. Additional bounded local handshakes
cover each classical group under TLS 1.2/1.3 and the six TLS 1.3/ECDSA TLS 1.2
suites. Hybrid-only peers fail in either direction. An ML-DSA server signed by a
classical CA completes a handshake with a native-default client, while the
policy client refuses its signing scheme. RSA suites and P-384/P-521/Ed25519
signing schemes receive inventory coverage only. These checks qualify the algorithm component; they do not
claim public configurations, session limits or service admission.

## Owned TLS signing qualification

The portable artifact runs signature DER boundary and hash-once verification,
shared-key transform-error/unwind retirement, retained-identity lifecycle and
local TLS 1.2/1.3 handshakes through the owned signer. Both configurations
refuse after shared-key retirement; preceding peer name/protocol refusals
leave it usable. Resumption is disabled in this fixture. Private state faults
are synthetic Rust failures, not native RNG/OOM/abort simulation. TLS roles,
sessions and resource qualification remain separate.

## Outbound TLS configuration qualification

The portable runner exercises the owned ClientConfig and ClockHandle through
local backend connections. Configuration inventory checks cover the fixed
protocol selector and disabled resumption, compression/cache, key logging,
secret extraction, early data and ticket requests; the provider remains
explicit and the process-global default unset. Both TLS versions complete
repeated full handshakes against a resumption-enabled peer, including absent
ALPN and HTTP/1.1 selection. Disjoint trust, wrong names and incompatible
ALPN fail (the non-overlap refusal occurs at the server; the client's check
of an unoffered selection has inventory coverage). A 16384-byte application
chunk emits exactly one TLS record. The client provider's groups, certificate
algorithms and complete handshake mappings are compared with the owned policy.

Clock fixtures cover ordinary missing time, recovery for another operation,
serialized shared callback panic/retirement and poisoned-handle refusal.
Backend verification fails with no supplied time in both versions. A TLS 1.3
peer completes Finished and then sends tickets after time becomes unavailable;
the client refuses despite its no-op resumption store. These configuration cases alone do not
qualify session error mapping or operations that do not request time; the
public session cases below exercise those additional boundaries.

A separate TLS 1.2 fixture returns transient None or panics exactly when the
backend saves session state after server Finished. The backend ignores the
time failure and completes the handshake; a later successful clock poll can
miss it. This pins a hazard, not an allowed public session success: M07c
retains each callback failure per connection and checks it and shared retirement
after backend work before publishing results. Normal TLS 1.2 session saving
also copies secret and certificate state before the no-op store drops it.

Verifier fixtures check certificate count, per-certificate and aggregate
ceilings before path work. Backend parsing occurs earlier and may allocate;
these are not whole-session memory bounds. Constructors, root copies, clocks,
verifiers and native connections remain subject to M07e allocation/stack/RSS
qualification. No listener or live network service is used.

## Inbound TLS configuration qualification

The portable runner constructs ServerConfig through admitted owned identities,
private trust and the shared clock. Fixtures pin accepted protocol/selection
combinations, identity/name limits (including 16 identities and 512 names),
global duplicate refusal, public-root refusal for client authentication and
fixed disabled backend features. Local TLS 1.2/1.3 peers prove distinct
certificate selection, required/default/match-present name policy, HTTP ALPN
absence/selection and full repeated handshakes against a resumption-enabled
client. A resumption-enabled server first seeds real client state and proves
a resumed baseline; a captured nonempty TLS 1.2 session ID or TLS 1.3 PSK
extension then reaches the td configuration and yields a fresh full handshake.
HTTP rejects an h2-only offer; SMTP selects no ALPN even when offered protocols.
No backend key reload or external network endpoint is used.

Mandatory client authentication accepts a valid private client and refuses
missing/foreign/expired/wrong-purpose/bad-signature certificates and excessive
certificate count/size. Supplied time controls cold local material and peer
verification. A configured CA bundle with more than 65535 bytes of encoded
subject hints still sends an empty hint list under both versions and still
requires a client certificate; omitted hints do not omit trust verification.

A raw ClientHello mutation pins the backend's conversion of an IP-literal SNI
into absence before resolver lookup. This is a known integration hazard, not
an accepted facade input: M07c validates raw SNI before that loss, checks
selected material and shared clock/key state before signing and after Finished,
and enforces the per-connection time-failure observation specified in TLS.md.
The configuration fixtures do not supply public session progress, gateway
leaf-pin/address policy, listener activation or complete resource bounds.

## Socket-free client session qualification

The runner uses the public client facade against local peers under TLS 1.2
and TLS 1.3. Two seven-byte pipes, one-byte reads and a 37-byte output tail
exercise fragmented handshakes, simultaneous 16384-byte writes and plaintext
backpressure. Fixtures cover exact complete-record and handshake-reassembly
limits, fixed redacted errors, no pre-Finished application/evidence, terminal
unwind consumption and healthy configuration reuse. The clock observer catches
an ignored TLS 1.2 post-Finished save failure and a TLS 1.3 ticket failure after
Finished, even when the source immediately recovers. Shared clock panic retires
other existing and new sessions; ordinary missing time remains per connection.

Close cases cover each version's independent halves, TLS 1.2 pending-output
alert refusal, caller-owned write tails, local-close bad MACs, peer-close
discard, truncated transport and writes after close. A requested TLS 1.3
KeyUpdate produces its response without application activity, preserving
previously queued ciphertext and subsequent key ordering. Both roles retire
the larger handshake output allowance only after Finished
and local flight drain. TLS 1.2/1.3 fixtures exercise the transition and a
synthetic backend-growth refusal using real encrypted records; healthy
configuration reuse must remain possible. Established ciphertext is capped
at 36874 bytes. This is a post-operation queue check, not an allocation bound.
The native deframer
retains record headers: a 65511-byte malformed handshake payload fits with
16384-byte fragments, while one extra byte exhausts capacity; 4096-byte
fragments lower that boundary to 65451. These fixtures qualify progress and
refusal semantics, not preallocation, server admission or service memory/RSS.

A TLS 1.3 ticket-flight case reaches authenticated Open state before consuming
opaque synthetic tickets with resumption disabled. Two 16000-byte or two
32000-byte tickets and single 48000-byte or 65000-byte tickets complete;
subsequent application bytes still decrypt at the peer. Two 48000-byte tickets
in the same backend flight exhaust retained intake capacity and permanently
retire the session, its clock owner, evidence and output. A healthy shared
configuration remains constructible. The backend retains completed-message
prefixes while the flight has an incomplete handshake message. These fixed
cases qualify progress/refusal, not an exact size boundary or memory peak.

An independently encrypted TLS 1.2 HelloRequest has a valid open-connection
warning-response baseline. After local close, with close ciphertext pending,
partially drained or fully drained, the same request instead retires the
session without drainable output. Test-only extraction is enabled on that
remote peer; facade configurations still disable extraction. Separate cases
pin the conservative simultaneous-close refusal when TLS 1.2 output remains
inside the facade or in the caller's socket-write tail.

## Socket-free server session qualification

The public server constructor begins with private bounded ClientHello admission.
The portable runner checks raw SNI before backend conversion, including every
fragment split, absent names, malformed/IP names, duplicate SNI and vector
lengths, large skipped extensions and retry consistency. Parser state is at
most 1024 bytes without an additional handshake copy. A valid forced HRR
baseline completes before mutated retry names are refused without evidence.
A separate absent-SNI first hello and IP-literal retry distinguishes facade
validation from the backend, which treats both names as absent. Mixed-case
initial names also reach every admitted server selection mode after folding.

Local public session pairs use seven-byte pipes, one-byte reads, short output
tails, simultaneous full-size writes and version-specific closure under both
TLS versions. Mandatory private-client cases verify the exact leaf fingerprint
against an independent digest and refuse missing/untrusted/expired/wrong-use/
bad-signature/count/size inputs. A future expiry case proves supplied time
reaches peer verification. Reusing healthy material after remote refusals is
required. Selected versus unrelated expiry/retirement, mid-handshake expiry,
established-date behavior, shared clock panic and consuming Acceptor unwind
are separate cases. No gateway authorization or whole-memory bound is claimed.

## Mail Rust allocation qualification artifact

`td-mta-rust-allocation-probe` is a separate integration test with its own main
and no libtest workers. The ordinary installed executable and library retain
their existing allocator. The build uses declared binutils nm to require the
`TD_MTA_ALLOCATION_COUNTERS` and `rust_alloc_probe` symbol-name substrings
in this test and refuse these sentinels in every other retained executable.
These are checks for the named probe, not exhaustive classification of all
allocator machinery; source confinement enforces its import boundary. The artifact inventory, static ELF checks and runtime
namespace include this executable explicitly. No common compiler/linker flag
changes. The runtime runs each mode in a fresh process. The default requires
its exact success line after model, forwarding and digest/SMTP checks. UNSAFE.md T1 and
td-mta/RESOURCES.md define the scope: requested Rust bytes only; no native,
RSS, whole-service or allocation-elimination guarantee is inferred.

## Mail native allocation diagnostic artifact

`td-mta-native-allocation-probe` is a seventh, separate integration executable.
Only its final Cargo rustc invocation receives `td_native_alloc_probe` and six
rust-lld `--wrap` arguments: malloc, calloc, realloc, free, posix_memalign and
aligned_alloc. Shared Rust/native flags and the installed executable are
unchanged. The normal host target prints an unqualified stub record; only the
portable run can produce the accepted forwarding/provider diagnostic record.

A const, drop-free native TLS guard suppresses nested libc calls, so requested
size and ownership follow the outermost call. Fixed state has 65536 registry
slots; its storage is test overhead, not service memory. Positive controls
exercise the six entry points, null arguments, successful zero-size ownership,
failed growth preserving storage, zeroing, alignment and output-pointer
preservation and four concurrent allocation/resize/free workers. A separate
process requires zero-size resize to invalidate evidence. RNG initialization
requires a positive provider boundary call and links native code for inspection.
Successful output reports registry bytes, fixed counter bytes and one thread
flag separately; thread TLS-block/runtime overhead is outside these figures.
The builder requires positive bounded values plus the exact completion record.
It requires wrapper/registry sentinels in this executable, refuses
any wrapper in the other seven executables and refuses resolved sdallocx or
OPENSSL_memory allocator hooks and unqualified aligned/array entry points. These checks do not classify all allocation
machinery or cover hidden/local references; counts remain diagnostic.

UNSAFE.md T2 and td-mta/RESOURCES.md define this observation. It does not replace
independent RSS/stack checks, establish TLS generation/session peak memory or
change the ledger. C boundary counts may include Rust System calls and must
never be added to Rust counts as disjoint native memory.

The two allocation executables also run `--tls-clients` in independent fresh
processes. The runtime validates all twelve ordered snapshot rows and their
completion records before printing observations. This exercises representative
public-root outbound policy generations and pending client construction through
the mail facade, without sockets. RESOURCES.md in td-mta defines columns,
positive controls and the limits of those diagnostic observations; they do not
qualify TLS session ceilings or whole-service RSS.

A separate `--tls-handshake` process per counter domain additionally qualifies
the local TLS 1.3 observation path. It generates synthetic material, completes
an admitted loopback pair, transfers verified 16 KiB records in both directions,
and requires stable retained requested bytes across repeated transfers. All
thirteen ordered rows are validated before logging. These representative
requested allocation observations do not establish TLS memory ceilings or RSS bounds.

Each domain additionally runs `--entropy-workers` in a fresh process. Seven
ordered rows record startup of eight test workers, cold RNG work on one then
all workers, repeated warmed fills, explicit joins including TLS destructors,
and final scope teardown. Warm fills must leave counters unchanged; native
cold RNG work must produce a positive malloc/calloc observation. Domain and
scenario logs are distinct and completion records are exact. Requested
bytes/blocks can remain after all workers exit. These diagnostics do not
measure worker stack mappings or RSS and do not change service admission.

Fresh `--tls-fragments` processes additionally exercise the socket-free facade's
existing malformed-handshake reassembly boundaries with 16384-byte and 4096-byte
fragments. Twelve ordered rows distinguish setup, retained incomplete input,
decoding/capacity refusals, repeated cycles and teardown. The pending snapshot
precedes the final record and does not isolate a transient peak. All rows and
completion markers must match their domain/scenario schema before logging.
This is an untrusted-input diagnostic, not a complete TLS/session/RSS bound.

The fresh `--tls-large-chain` processes reuse the local authenticated TLS 1.3
scenario with a signed leaf/intermediate/root chain above 60 KiB of PEM but
within the local loader's 64 KiB limit. Each padded certificate remains within
16 KiB DER. Thirteen ordered observations and exact completion use a separate
large-chain schema and log per domain. Both endpoints and retained fixture
material are included; successful large-chain traffic is not a total service,
maximum-profile or RSS qualification.

Handshake and large-chain allocation records use schema v2 with separate
client, remaining-server and returned-wire-buffer release checkpoints. The
counter probes require exactly 73748 requested bytes to disappear when the
four returned buffers drop; native tracking additionally requires four fewer
blocks. Other allocation scenarios retain v1. Endpoint release deltas exclude
shared state and stack storage.

Fresh `--tls-generations` processes in both counter domains compile sixteen
large files profiles into direct SMTP, HTTPS/MTA-STS and relay policies, retain
old and replacement owners, refuse a third generation, and repeat replacement
after releasing the old lease. Eleven ordered rows and a distinct v1 completion
are required. Counter refusal must be unchanged and repeated replacements must
not retain requested bytes or native blocks. This is a profile-count fixture,
not a complete generation-memory bound; td-mta/RESOURCES.md defines its scope.

## Mail sampled RSS diagnostic artifact

`td-mta-rss-probe` is an eighth integration executable with its own main.
It uses the ordinary allocator, no native wrappers and no allocation-counter
storage. Symbol checks require the `rss_probe` sentinel only in this artifact
and refuse the Rust/native probe sentinels there. The exact artifact inventory,
static ELF validation and fresh runtime namespace include it explicitly.

The default process checks the bounded rollup parser and observes a positive
resident increase after touching a 16 MiB allocation. Nine independent
fresh processes run the client, local handshake, entropy worker, fragmented
handshake, large-chain, sixteen-profile generation, large generation-routing,
gateway-trust generation and decoded certificate-list refusal scenarios. The
runtime requires exact ordered rows, positive decimal KiB values and the
scenario completion record before
logging. Every mode has its own bounded log and 30-second deadline. Missing
procfs support or malformed/truncated rollup output fails qualification.

RESOURCES.md in td-mta defines the observation scope. These are sampled whole
fixture process values, including stacks and observer state. They do not
establish transient peaks, isolate provider costs or qualify service admission.

RSS completion records use v2 for every scenario, with the same additional
handshake release phases. The runner rejects earlier completion versions and
missing/reordered phases. An RSS delta is not required to match a requested
allocation delta.

## Remote peer-chain memory observations

The existing unwrapped mail test executable additionally controls nine local
process fixtures: TLS 1.2, TLS 1.3 with two tickets, and TLS 1.3 with one large
ticket, each with a fresh Rust, native or RSS observer. A private test-only
crypto server reads four synthetic DER objects whose total exceeds 63 KiB and
fits 65000 bytes, independently of the local PEM loader ceiling. TLS 1.3 emits
either two counted opaque 16000-byte test tickets or one 65000-byte ticket;
the private ticketer refuses resumption and performs no encryption. Controller
and peer allocations are outside each observer; native instrumentation never
crosses a fork. Explicit paths select only the existing qualification
executables, and no artifact, link flag or API is added.

Each controller requires the complete authenticated handshake, verified record
exchange and bounded child completion. Owned guards clean up on normal return
and unwinding; outer hard kills rely on runtime namespace cleanup. The runtime
validates unique observation markers, v2 domain/scenario completion, one
successful test summary and eleven ordered inner rows. It refuses missing,
duplicate or wrong-version records before printing diagnostics. RESOURCES.md
defines attribution and limits: this is remote-chain coverage, not total
session admission or an isolated maximum ticket-processing peak.

## Large generation routing observations

Both allocation executables and the unwrapped RSS executable additionally run
`--tls-generation-routing` in fresh processes. It combines sixteen profiles,
sixteen listeners and 256 MTA-STS domains, including 255 long derived names,
while retaining two generations and repeating replacement. Every local chain
still obeys the PEM/DER ceilings. The runtime requires distinct ordered
`generation-routing` records and completion markers and rejects evidence from
the smaller generation case. No new executable, unsafe surface or link flag
is introduced. RESOURCES.md defines retained fixture attribution, checked
release/refusal invariants and the remaining aggregate-admission limits.

## Gateway trust generation observations

Both allocation executables and the unwrapped RSS executable additionally run
`--tls-generation-trust` in fresh processes. It constructs sixteen admitted
large file profiles, fifteen gateway policies with full 128-anchor private
bundles, one HTTPS listener and relay trust. The fixture's explicit fifteen
SMTP slots, 512 KiB disposable index cache and 128 MiB planner budget form
a fixture profile, not a change to shipped concurrency/cache defaults. Its
external MX configuration needs no direct SMTP listener. This measures cold
configuration retention without claiming peer authentication.

The runtime requires exact eleven-phase `generation-trust` records and the
v1 allocation or v2 RSS completion before printing diagnostics. Other scenario
records cannot satisfy this evidence. Stable old-generation release and
repeated replacement remain allocation oracles. No executable, unsafe surface
or link flag is added. td-mta/RESOURCES.md defines the allocation guards,
their attribution limits and pending aggregate admission work.

## Certificate-list refusal observations

Fresh `--tls-certificate-list` modes in the existing Rust/native/RSS
artifacts send an unexpected plaintext Certificate message in the TLS 1.2
format before ServerHello. Its 21800 empty DER entries fit in a bounded
65403-byte body, delivered as either 16 KiB or 4 KiB record fragments. The
pinned backend decodes the list before refusing the unexpected message. No
certificate or peer is authenticated.

Eleven exact `certificate-list` phase records and allocation v1/RSS v2
completions distinguish these observations from malformed ServerHello tests.
Allocation probes require the first final-record step to exercise at least
512 KiB of additional requested-byte peak above its pending peak snapshot;
this qualifies the current pinned decoder path and is not a desired minimum
for a future implementation. It relies on the pin's vector growth and spare
capacity. Repeated refusals must retain no extra requested bytes or native
blocks. Terminal failure must discard output and refuse further operations.
No executable, unsafe surface or link flag is introduced. RESOURCES.md
defines the attribution limits and remaining admission work.

## Mail SQLite qualification boundary

The staged mail lock now adds private rusqlite and bundled SQLite sources;
the shared vendor includes that exact checksummed closure. Existing native
compiler/sysroot restrictions, static linking, frame pointers, debug companions
and path remapping apply to its amalgamation. The shared controls force
LIBSQLITE3_SYS_USE_PKG_CONFIG=0 and the native flags specified in DEPENDENCIES.md;
unexpected ambient SQLite controls fail admission. No SQLite tool or system
library is a build input. td-crypto's active dependency graph is unchanged.

Earlier portable crypto/TLS, native-allocation, stack and RSS observations do
not qualify SQLite. A fresh isolated mail build and native SQLite resource,
crash/fault and combined-owner measurements are required before packaging an
activated persistence service. Host SQLite tests are not target evidence.

The mail Rust/native/RSS executables also accept --sqlite-body. This scenario
is registered in the isolated runtime and passed the isolated qualification
recorded below.
It generates an exact 32 MiB body in 64 KiB chunks, commits it, verifies
bytes/digest, reopens, rejects a short source and an oversized source, and
successfully reuses the rolled-back ID. Eight ordered observations bracket
opening, midpoint streaming, commit, verification, reopen, rejection and
teardown. No whole-message allocation enters the source or oracle.

The acceptance thresholds require a Rust requested-byte
high-water increase of at most 2 MiB and teardown return to its warmed
baseline. They require wrapped native requested-byte growth of at most
17 MiB (SQLite's 16 MiB shared heap plus 1 MiB C runtime allowance), positive
allocation evidence and exact warmed byte/block return. Unwrapped RSS
samples at those eight points must grow by at most 24 MiB. These are test
requirements; adding the probes alone establishes no measurement. RSS samples
do not establish a transient peak; native counters observe
wrapped allocation lifetime high-water separately. Host native builds remain
UNQUALIFIED; only the isolated static-musl wrappers can qualify their native
observations. Guarded stack and combined-service workload remain separate.

The additional --sqlite-account mode runs in separate fresh Rust,
wrapped-native and RSS processes. It commits the same streamed 32 MiB
body plus parent/child mailboxes, retains the existing body verification
and refusal/reuse controls, then checks complete account metadata and
bodies through a maintenance view before and after reopen. Nine phases
add account_verified after verified. Its 2 MiB Rust, 17 MiB wrapped
C-boundary and 24 MiB sampled RSS growth ceilings match the body
scenario; allocation domains must return to their warmed baselines and
the account pass must produce positive wrapped C-boundary allocation
evidence. Rust observations retain their existing positive controls.
Exact account prefixes, ordered phases and completion markers refuse
cross-scenario evidence. This bounded fixture alone does not qualify
arbitrary-account metadata, maximum-database maintenance, guarded stack,
complete filesystem faults or whole-service overlap. Adding the mode
does not establish a portable measurement until the isolated command
succeeds.

The separate --sqlite-backup mode runs in fresh Rust, wrapped C-boundary
and unwrapped RSS processes. It commits the same streamed 32 MiB body
and parent/child mailboxes, retains body and account verification, then
consumes the source through public offline backup with caller-owned
64 KiB scratch. It reopens both roots for physical integrity and complete
account metadata/body checks. Both two-reader stores plus their writers
remain live at restored_verified. Ten ordered phases add backed_up,
source_verified and restored_verified on this separate copy path;
existing body/account modes retain their refusal/reuse controls and
original phase sequences. Growth limits remain 2 MiB Rust requested,
17 MiB wrapped C-boundary requested and 24 MiB sampled RSS. Exact warmed
teardown and positive forwarding/native account controls remain required.
Distinct prefixes and completion markers reject cross-scenario evidence;
each new observation process has the existing 300-second bound.

The separate --sqlite-epoch mode adds public consuming epoch renewal to the
real body/mailbox backup path, with actual SystemEntropy warmed on the same
observing thread before baseline. Its counted wrapper requires exactly one
16-byte renewal fill; the returned destination identity must differ only in
epoch. Positive old/new passive state controls and destination checkpoint/
reopen require the new identity to persist and old state to remain stale;
the original source must retain its original identity and old state.
Physical integrity and complete account metadata/body checks run in each
root, after renewal, after destination reopen and again on the source.
Both root locks and the source's two-reader/writer store remain held while
the destination's two-reader/writer store is renewed and reopened.

Thirteen ordered phases use distinct sqlite-epoch prefixes/completion:
baseline, opened, writing, committed, verified, account_verified, backed_up,
source_verified, restored_verified, epoch_renewed, epoch_reopened,
source_preserved and dropped. The three existing modes retain their original
phase layouts and controls. All three observer entrypoints use the shared
mode; no new allocation instrumentation or linker wrapping is added.
The driver runs separate fresh Rust/native/RSS processes with the existing
300-second bound. Exact evidence parsing rejects other SQLite scenarios and
missing, duplicated or reordered phases.

The complete isolated x86-64 musl qualification on 2026-10-09 passed on its
first attempt, including all eight static artifact checks, API confinement
and the clean runtime with all previous modes plus the epoch mode. It used
pinned Rust 1.96.0, declared GNU tools and unchanged SQLite
9 MiB per-allocation/16 MiB process-wide requested-allocation caps.
The process-wide limit is shared by both overlapping pools. Measurements were:

| Observer | Warm baseline | Lifetime requested peak / maximum RSS sample | After teardown |
| --- | ---: | ---: | ---: |
| Rust requested bytes | 880 | 330164 | 880 |
| Wrapped C boundary requested bytes | 134584 (13 blocks) | 2122068 | 134584 (13 blocks) |
| Unwrapped RSS, KiB | 4300 | 6528 | 4328 |

All three paths passed the unchanged 2 MiB Rust, 17 MiB C-boundary and
24 MiB sampled RSS growth ceilings. Both allocation domains returned exactly
to their warmed live baselines; positive forwarding/body/account controls
remain. The warm baseline includes provider thread/global state; dropping
the handle does not release that state. Retained source and destination
stores overlap at restored_verified, epoch_renewed, epoch_reopened and
source_preserved. Call/view/verification intervals do not isolate method
allocations, and C counts may include Rust System calls. These samples do
not establish transient RSS, guarded stack, maximum database/account,
entropy quality or worst-case latency, unknown outcomes, full filesystem
faults, power loss, whole-service resources or operational restore activation.

The epoch artifact NAR was
671d4bf8f5442f8c8cbc036f1fd93a78ff1262b58f5ba6a9d7701b85ed56c7b8.
Its BUILD-INPUTS records staged source NAR
b6f95f848ef01da6104a5d5404dd0a06f08dcc22f3a7a766ac007378347b86ed
and unchanged vendor NAR
d122b8e7843f7da35cfb1f393dd0d8aea43ef534e9f5cffddc77a095fe971916.

The complete isolated pinned Rust 1.96.0 release x86-64 musl command
passed on its first attempt on 2026-10-09: all eight static artifacts,
API confinement, clean runtime, all resource and positive controls and
four guarded worker-stack cases. No production API, unsafe boundary,
syscall, dependency, reservation, cap or compiler flag changed.

The shared portable body/account/backup/epoch fixtures also passed a
public usage fence with all eight partial body inputs retained. After
the metadata PUT commits sequence two, the fence captures exact passive
totals: the current epoch, one account, 32 MiB body bytes, one blob and
zero upload bytes, queue bytes and queue submissions. Main-file and WAL
extents are positive and within their respective public
SQLITE_DATABASE_BYTES and SQLITE_WAL_BYTES limits; the WAL limit
includes frame overhead. With the fence alive, another metadata PUT at
expected sequence two, checkpoint, a second fence and a ninth view all
refuse Busy.

The fence remains alive through the remaining 511 chunks of each input,
all eight complete original-body digests, pinned random reads and the
verified observation. Dropping the fence precedes dropping pins and old
views. All eight old views retain sequence one and original typed
metadata; all eight reacquired views expose sequence two and the updated
parent. Thus the refused fenced commit does not advance the endpoint.
All original selectors, phase counts, owner counts, copy/reopen/epoch
checks and rollback/refusal/retry controls remain.

| Case | Observer | Warm baseline | Lifetime requested peak / maximum RSS sample | After teardown |
| --- | --- | ---: | ---: | ---: |
| Body | Rust requested bytes | 560 | 199846 | 560 |
| Body | Wrapped C boundary requested bytes | 1344 | 2943766 | 1344 |
| Body | Unwrapped RSS, KiB | 3364 | 6856 | 3592 |
| Account | Rust requested bytes | 624 | 199910 | 624 |
| Account | Wrapped C boundary requested bytes | 1424 | 2943846 | 1424 |
| Account | Unwrapped RSS, KiB | 3364 | 6856 | 3596 |
| Backup | Rust requested bytes | 688 | 333620 | 688 |
| Backup | Wrapped C boundary requested bytes | 1504 | 4197372 | 1504 |
| Backup | Unwrapped RSS, KiB | 3360 | 8272 | 3616 |
| Epoch | Rust requested bytes | 880 | 333812 | 880 |
| Epoch | Wrapped C boundary requested bytes | 134584 | 4330452 | 134584 |
| Epoch | Unwrapped RSS, KiB | 4084 | 8868 | 4212 |

Writable worker mappings before/after were body 253952/253952 bytes,
account 253952/253952 bytes, backup 253952/253952 bytes, epoch
253952/253952 bytes. All have the required adjacent inaccessible guard
and no grow-down flag, with explicit joins before completion. Native
warm/final block counts remain five for body/account/backup and thirteen
for epoch; the warmed entropy handle stays alive. Wrapped C observations
are not SQLite-only attribution.

This qualifies passive usage capture and bounded refusals with eight
same-account/same-body loans in one process or guarded worker.
initialize_leases is not called: these totals grant no quota
reservation, effect authorization or service quiescence. Whole-fixture
peaks and exact warm teardown do not isolate usage_fence allocations or
prove per-call allocation freedom. The complete suite also passes its
separate unchanged quiet allocation controls. Rust 2 MiB, wrapped
C-boundary 17 MiB, sampled RSS growth 24 MiB and guarded writable
mapping 256 KiB limits remain unchanged, as do SQLite 9 MiB
per-allocation and 16 MiB process-wide caps across all pools. No
parallel-thread, multi-account, maximum-database, transient-RSS, frame
high-water, full-fault, power-loss or whole-service claim follows.

The artifact NAR was
7e8a120c34cc6a9de0350842526588e37ab386919760bfd985973ebc33bdcd92.
Its BUILD-INPUTS records staged source NAR
1e2f27896b816d9d47963754e1885f6357ec8b557454c4ffc1f0a152d3029181
and unchanged vendor NAR
d122b8e7843f7da35cfb1f393dd0d8aea43ef534e9f5cffddc77a095fe971916.

The complete isolated pinned Rust 1.96.0 release x86-64 musl command
passed on its first attempt on 2026-10-09: all eight static artifacts,
API confinement (schema 57, 29 fixtures, 786 reachable items), clean
runtime, all existing resource/positive/quiet controls, seven SQLite
allocation/RSS cases and seven guarded-stack workers. Builder parsers
accept the exact seventh selector and fifteen-phase protocol, including
pools_pruned then pools_verified before dropped. Allocation, RSS and
stack negative matrices cover all seven selectors. These measurements
are from a fresh seven-selector run; prior artifact measurements remain
historical.

A separate portable sqlite-pruned-overlapping-accounts selector
qualifies bounded history retirement and cleanup while maximum-size body
loans remain open. It keeps the earlier six selectors independently
runnable with phase counts 8/9/10/13/13/14 and adds a seventh
fifteen-phase case. The two public accounts share BlobId and
parent/child MailboxId values, with independently expected distinct 32
MiB bodies, digests and names: 64 MiB logical body bytes per root. Only
the seventh case appends a parent Created CHANGE at cursor (1,3) to each
initial three-PUT commit and an account-A Updated CHANGE at cursor (2,1)
to the parent update.

Eight alternating source views at sequence one/floor zero hold eight
partially read body inputs through the account-A update to sequence two
and passive usage-fence checks. Drop that fence only in the seventh
case, then publicly prune A through sequence two with a one-row budget
while all eight partial inputs remain alive: exactly one row removed,
floor two and more true. Interleave the remaining body chunks, verify
every account-specific byte and complete digest, retain all eight pins
through boundary/final-byte checks, then confirm the old views still
retain their exact Created history and initial typed metadata. Current A
has sequence two/floor two and refuses lost history; B stays sequence
one/floor zero with its exact Created record and completion.

Actual public backup preserves that pending A cleanup independently in
source and copy. Complete physical/account verification, one actual
warmed SystemEntropy fill with independently captured expected copied
epoch, durable copied reopen and preserved source domain precede
overlap. Both complete eight-reader pools then retain sixteen
alternating views, eighteen native owners and sixteen 32 MiB inputs
together. Every view checks explicit source/copied epoch, account,
endpoint/floor, original typed BlobRow and changed sequence, full
parent/child metadata and exact history. All sixteen inputs read their
first 64 KiB before copied A then source A independently perform one-row
cleanup: receipts remove one then zero rows, both more false, under each
expected epoch. Both stores refuse ninth views and checkpoints. A
pools_pruned observation occurs with all sixteen partial inputs alive.

Interleave the remaining 511 chunks per input, compare every expected
byte and verify complete digests through finish. Retain all sixteen
completed pins through cross-chunk/final-byte reads, both Busy controls
and pools_verified. After pin release, repeat all sixteen typed
identity/history checks and Busy controls. Release views, checkpoint
both roots, pass physical checks and repeat complete independent account
verification including retained/lost history. One caller 64 KiB scratch
is reused; no whole-body buffer is introduced.

| Case | Observer | Warm baseline | Lifetime requested peak / maximum RSS sample | After teardown |
| --- | --- | ---: | ---: | ---: |
| Body | Rust requested bytes | 560 | 199846 | 560 |
| Body | Wrapped C boundary requested bytes | 1344 | 2943766 | 1344 |
| Body | Unwrapped RSS, KiB | 3412 | 6980 | 3716 |
| Account | Rust requested bytes | 624 | 199910 | 624 |
| Account | Wrapped C boundary requested bytes | 1424 | 2943846 | 1424 |
| Account | Unwrapped RSS, KiB | 3416 | 6984 | 3724 |
| Backup | Rust requested bytes | 688 | 333620 | 688 |
| Backup | Wrapped C boundary requested bytes | 1504 | 4197372 | 1504 |
| Backup | Unwrapped RSS, KiB | 3416 | 8400 | 3744 |
| Epoch | Rust requested bytes | 880 | 333812 | 880 |
| Epoch | Wrapped C boundary requested bytes | 134584 | 4330452 | 134584 |
| Epoch | Unwrapped RSS, KiB | 4072 | 8928 | 4272 |
| Multi-account | Rust requested bytes | 880 | 333812 | 880 |
| Multi-account | Wrapped C boundary requested bytes | 134584 | 4330452 | 134584 |
| Multi-account | Unwrapped RSS, KiB | 4076 | 8932 | 4276 |
| Overlapping-accounts | Rust requested bytes | 944 | 333876 | 944 |
| Overlapping-accounts | Wrapped C boundary requested bytes | 134664 | 5959652 | 134664 |
| Overlapping-accounts | Unwrapped RSS, KiB | 4072 | 10536 | 4296 |
| Pruned-overlapping-accounts | Rust requested bytes | 1008 | 335332 | 1008 |
| Pruned-overlapping-accounts | Wrapped C boundary requested bytes | 134744 | 5965244 | 134744 |
| Pruned-overlapping-accounts | Unwrapped RSS, KiB | 4072 | 10564 | 4272 |

Writable worker mappings before/after were body 253952/253952 bytes,
account 253952/253952 bytes, backup 253952/253952 bytes, epoch
253952/253952 bytes, multi-account 253952/253952 bytes,
overlapping-accounts 253952/253952 bytes, pruned-overlapping-accounts
249856/249856 bytes. Each has its adjacent inaccessible guard, no
grow-down flag and explicit join. Warm/final native block counts were
body 5/5, account 5/5, backup 5/5, epoch 13/13, multi-account 13/13,
overlapping-accounts 13/13, pruned-overlapping-accounts 13/13. The
warmed entropy handle remains alive through teardown.

This qualifies fixed two-account public history retirement/cleanup with
eight initial partial maximum-size loans and sixteen copied/source
partial maximum-size loans. It does not make the earlier sixth selector
a pruning case or replace the separate native 2 MiB fixture. Lifetime
requested peaks and exact warm teardown do not prove per-call allocation
freedom. Wrapped C observations may include Rust System allocations and
are not disjoint SQLite-only attribution. Named RSS samples include
sixteen partial inputs and sixteen completed pins but do not bound
transient RSS; the second initial body write has no separate writing
sample. Writable mapping size is not frame high-water. Limits remain
Rust 2 MiB, wrapped C-boundary 17 MiB, sampled RSS growth 24 MiB,
guarded writable mapping 256 KiB, SQLite per-allocation 9 MiB and
process-wide 16 MiB across all pools. No
arbitrary-account/maximal-database resource, parallel-service, quota
reservation, effect authorization, initialize_leases, quiescence, full
filesystem-fault, power-loss or whole-service claim follows. No
production API, schema, unsafe surface, syscall, hook, allowance,
dependency, cap, compiler flag, probe shim or stack wrapper changed.

The artifact NAR was
dc3d0bdfa11f3cf9af905956bf7d08e896a6d1515470ff2c84199e4dc7464f11.
Its BUILD-INPUTS records staged source NAR
bd38317bf5c2f2e2c909b354cb2ffbfac80987dbc2cc8b5eedc4a1eac583da61
and unchanged vendor NAR
d122b8e7843f7da35cfb1f393dd0d8aea43ef534e9f5cffddc77a095fe971916.

The complete isolated pinned Rust 1.96.0 release x86-64 musl command
passed on its first attempt on 2026-10-09: all eight static artifacts,
API confinement (schema 57, 29 fixtures, 786 reachable items), clean
runtime, all existing resource/positive/quiet controls, six SQLite
allocation/RSS cases and six guarded-stack workers. Builder parsers
accept the exact sixth selector and fourteen-phase protocol, with
allocation/stack cross-scenario negatives and a six-scenario RSS matrix
refusing missing/duplicated/reordered phases, malformed values and
completion records. These measurements are from a fresh six-selector
run; prior artifact measurements remain historical.

A separate portable sqlite-overlapping-accounts selector extends the
two-account maximum-body case with sixteen simultaneous loans in the
source and its renewed copy. Both accounts share BlobId and parent/child
MailboxId values but have independently expected distinct 32 MiB body
bytes, digests and names: 64 MiB logical body bytes per root. The first
account stays at sequence two, the second at one and both at floor zero.
Public backup, complete physical/account verification, one actual warmed
SystemEntropy fill with independently captured expected output, durable
copied epoch reopen and preserved original source remain prerequisites.
The five previous selectors remain separately runnable with phase counts
8/9/10/13/13; this sixth selector adds pools_verified before dropped for
fourteen observations.

Both complete eight-reader pools retain sixteen views and eighteen
native owners together, alternating four views per account in each root.
Every view checks its explicitly expected source/copied epoch, account,
endpoint and floor, original typed BlobRow and changed sequence,
parent/child names, relationship and changed sequences before and after
the loans. All sixteen inputs read their first 64 KiB before any
completes, then interleave the remaining 511 chunks, checking every
expected byte and completed digest. All sixteen pins remain alive
through cross-chunk/final-byte checks and the pools_verified
observation. Both stores refuse ninth views and checkpoints while loans
or views remain. After views drop, both checkpoint, pass physical
validation and repeat complete account verification with independent
byte and typed-row expectations. The caller reuses one 64 KiB scratch
buffer throughout; no whole-body buffer is introduced.

| Case | Observer | Warm baseline | Lifetime requested peak / maximum RSS sample | After teardown |
| --- | --- | ---: | ---: | ---: |
| Body | Rust requested bytes | 560 | 199846 | 560 |
| Body | Wrapped C boundary requested bytes | 1344 | 2943766 | 1344 |
| Body | Unwrapped RSS, KiB | 3396 | 6900 | 3636 |
| Account | Rust requested bytes | 624 | 199910 | 624 |
| Account | Wrapped C boundary requested bytes | 1424 | 2943846 | 1424 |
| Account | Unwrapped RSS, KiB | 3396 | 6900 | 3644 |
| Backup | Rust requested bytes | 688 | 333620 | 688 |
| Backup | Wrapped C boundary requested bytes | 1504 | 4197372 | 1504 |
| Backup | Unwrapped RSS, KiB | 3400 | 8320 | 3664 |
| Epoch | Rust requested bytes | 880 | 333812 | 880 |
| Epoch | Wrapped C boundary requested bytes | 134584 | 4330452 | 134584 |
| Epoch | Unwrapped RSS, KiB | 3920 | 8780 | 4124 |
| Multi-account | Rust requested bytes | 880 | 333812 | 880 |
| Multi-account | Wrapped C boundary requested bytes | 134584 | 4330452 | 134584 |
| Multi-account | Unwrapped RSS, KiB | 3924 | 8780 | 4124 |
| Overlapping-accounts | Rust requested bytes | 944 | 333876 | 944 |
| Overlapping-accounts | Wrapped C boundary requested bytes | 134664 | 5959652 | 134664 |
| Overlapping-accounts | Unwrapped RSS, KiB | 3920 | 10384 | 4144 |

Writable worker mappings before/after were body 253952/253952 bytes,
account 253952/253952 bytes, backup 253952/253952 bytes, epoch
253952/253952 bytes, multi-account 253952/253952 bytes,
overlapping-accounts 253952/253952 bytes. Each has its adjacent
inaccessible guard, no grow-down flag and explicit join. Warm/final
native block counts were body 5/5, account 5/5, backup 5/5, epoch 13/13,
multi-account 13/13, overlapping-accounts 13/13. The warmed entropy
handle remains alive through teardown.

This qualifies this bounded two-account source/copied fixture with
sixteen maximum-size loans in one process or guarded worker. It performs
no history pruning or write while these sixteen copied/source loans
remain, and does not extend the separate native 2 MiB cleanup fixture
into portable pruning qualification. Lifetime requested peaks and exact
warm teardown do not prove per-call allocation freedom. Wrapped C
observations can include Rust System allocations and are not disjoint
SQLite-only attribution. RSS includes a sample with all sixteen
pins/views retained but does not bound transient RSS; the second initial
body write still has no separate writing sample. Writable mapping size
is not frame high-water. The unchanged limits are Rust 2 MiB, wrapped
C-boundary 17 MiB, sampled RSS growth 24 MiB, guarded writable mapping
256 KiB, SQLite per-allocation 9 MiB and process-wide 16 MiB across all
pools. No arbitrary-account/maximal-database resource, parallel-service,
quota reservation, effect authorization, initialize_leases, quiescence,
full filesystem-fault, power-loss or whole-service claim follows. No
production API, schema, unsafe surface, syscall, hook, allowance,
dependency, cap, compiler flag, probe shim or stack wrapper changed.

The artifact NAR was
1792bf95795f99b03dd86e331951eadee6f53118b8a51a70285b54e0b8863933.
Its BUILD-INPUTS records staged source NAR
5e0d6f44c9fd66a3093c972beddfe14d7d3a728963e55cb08f5027b7ac91b5ee
and unchanged vendor NAR
d122b8e7843f7da35cfb1f393dd0d8aea43ef534e9f5cffddc77a095fe971916.

The complete isolated pinned Rust 1.96.0 release x86-64 musl command
passed on its first attempt on 2026-10-09: all eight static artifacts,
API confinement (schema 57, 29 fixtures, 786 reachable items), clean
runtime, all existing resource/positive/quiet controls, five SQLite
observer cases and five guarded worker-stack cases. The four earlier
body/account/backup/epoch selectors remain independently runnable with
their original phase counts; the new selector has thirteen phases.
Explicit body and typed-row expectations also strengthen the existing
account verification paths, and epoch renewal now compares the actual
supplied entropy output. These measurements are a fresh run of all five
selectors, not a reuse of earlier artifact evidence.

A separate portable sqlite-multi-account selector now qualifies two
public accounts sharing the same BlobId and parent/child MailboxId
values. Each has a distinct uniform 32 MiB body, digest and mailbox
names: 64 MiB total body bytes. Eight source views alternate four
captures per account, retain eight partial body inputs across a
first-account metadata PUT, and finish every byte, complete digest and
pinned read under a passive usage fence. The fence reports two accounts,
two blobs and exact 64 MiB body totals, zero upload/queue bytes and
submissions, and positive bounded main/WAL extents. Both accounts start
at sequence one and floor zero; the first advances to sequence two while
the second remains at one. Another commit, checkpoint, second fence and
ninth view refuse Busy while the fence is held. Old views preserve their
original metadata; reacquired views show the account-specific endpoints
and names.

The fixture consumes the source through actual public backup with a
positive capped receipt covering at least 64 MiB. Source and copy retain
both complete eight-reader pools and writers together: eighteen native
owners, without retaining sixteen source/copy views together. Physical
verification and complete account verification cover both accounts in
each store. Independent expected mailbox rows and changed sequences,
every expected account-specific body byte, and exact BlobRow length,
digest and changed sequence prevent a self-consistent cross-account
body/metadata swap from passing. Copied public renewal uses one
sixteen-byte fill from its actual warmed SystemEntropy handle; the
returned epoch must equal the independently captured supplied bytes and
differ from the original. Both copied state domains adopt that epoch, it
survives checkpoint/close/reopen, and both original source state domains
and contents remain unchanged.

| Case | Observer | Warm baseline | Lifetime requested peak / maximum RSS sample | After teardown |
| --- | --- | ---: | ---: | ---: |
| Body | Rust requested bytes | 560 | 199846 | 560 |
| Body | Wrapped C boundary requested bytes | 1344 | 2943766 | 1344 |
| Body | Unwrapped RSS, KiB | 3500 | 6992 | 3728 |
| Account | Rust requested bytes | 624 | 199910 | 624 |
| Account | Wrapped C boundary requested bytes | 1424 | 2943846 | 1424 |
| Account | Unwrapped RSS, KiB | 3500 | 6996 | 3736 |
| Backup | Rust requested bytes | 688 | 333620 | 688 |
| Backup | Wrapped C boundary requested bytes | 1504 | 4197372 | 1504 |
| Backup | Unwrapped RSS, KiB | 3504 | 8416 | 3760 |
| Epoch | Rust requested bytes | 880 | 333812 | 880 |
| Epoch | Wrapped C boundary requested bytes | 134584 | 4330452 | 134584 |
| Epoch | Unwrapped RSS, KiB | 4352 | 9136 | 4480 |
| Multi-account | Rust requested bytes | 880 | 333812 | 880 |
| Multi-account | Wrapped C boundary requested bytes | 134584 | 4330452 | 134584 |
| Multi-account | Unwrapped RSS, KiB | 4348 | 9112 | 4476 |

Writable worker mappings before/after were body 253952/253952 bytes,
account 253952/253952 bytes, backup 253952/253952 bytes, epoch
249856/249856 bytes, multi-account 253952/253952 bytes. Each has the
required adjacent inaccessible guard and no grow-down flag, and joins
before completion. Warm/final native block counts were five for
body/account/backup and thirteen for epoch/multi-account; the warmed
entropy handle stays alive.

This qualifies these two accounts and shared IDs in one process or
guarded worker, without history pruning. It does not qualify arbitrary
account counts, maximum-database resource use, parallel services, quota
reservation, effect authorization, initialize_leases, service
quiescence, full filesystem faults, power loss or whole-service
readiness. Lifetime requested peaks and exact warm teardown do not
isolate per-call allocations or assert an allocation-free SQLite
interval. Wrapped C observations can include Rust System allocations and
are not disjoint SQLite-only attribution. RSS is sampled at named
phases, including one first-body writing sample; it does not bound
transient RSS or separately sample the second body during writing.
Writable mapping size is not a frame high-water measurement. Rust 2 MiB,
wrapped C-boundary 17 MiB, sampled RSS growth 24 MiB and guarded
writable mapping 256 KiB limits remain unchanged, as do SQLite 9 MiB
per-allocation and 16 MiB process-wide caps across all pools. No
production API, schema, unsafe surface, syscall, dependency, compiler
flag, probe shim or stack wrapper changed.

The artifact NAR was
05daa0360d389694d88587a7fbfcae78997e77ff58cf1561884692ec3a642718.
Its BUILD-INPUTS records staged source NAR
b05bd725efbcfe0e0d619f1691b2d327eeaf5f70ceb057f9696f68769de4b16b
and unchanged vendor NAR
d122b8e7843f7da35cfb1f393dd0d8aea43ef534e9f5cffddc77a095fe971916.

The retained-reader metadata-writer isolated release x86-64 musl
qualification on 2026-10-09 passed the complete portable command on its
first attempt: all eight static artifacts, API confinement, clean
runtime, all existing resource and positive controls, and four guarded
worker-stack cases. Pinned Rust 1.96.0 and declared GNU inputs remain
unchanged.

The shared portable SQLite body/account/backup/epoch fixtures passed a
public metadata commit with eight retained partial body inputs. After
the initial 32 MiB body commit at sequence one, all eight views capture
the same full identity and each input reads its first exact 64 KiB
chunk. With every input and view retained, the writer puts the parent
mailbox with name updated parent and commits sequence two. Body mode
creates that parent; account/backup/epoch modes rename their existing
parent, preserving the three-row/two-mailbox dataset. Ninth captures
refuse Busy before and after this commit. The committed observation now
occurs after the metadata commit with all eight partial inputs alive.

The remaining 511 chunks per input progress sequentially in round-robin
order through the same caller scratch. Every original body byte and each
completed digest is checked; all eight pins pass cross-chunk and
final-byte reads and remain alive with their views at the verified
observation. After dropping pins, all eight old views retain their
complete sequence-one identities and the original parent row at changed
sequence one, or no parent in body mode. Releasing them permits
simultaneous reacquisition of all eight slots with full sequence-two
identities and the exact updated parent row at changed sequence two.
Those views drop before account verification or consuming backup.

| Case | Observer | Warm baseline | Lifetime requested peak / maximum RSS sample | After teardown |
| --- | --- | ---: | ---: | ---: |
| Body | Rust requested bytes | 560 | 199846 | 560 |
| Body | Wrapped C boundary requested bytes | 1344 | 2943734 | 1344 |
| Body | Unwrapped RSS, KiB | 3356 | 6848 | 3584 |
| Account | Rust requested bytes | 624 | 199910 | 624 |
| Account | Wrapped C boundary requested bytes | 1424 | 2943814 | 1424 |
| Account | Unwrapped RSS, KiB | 3352 | 6848 | 3588 |
| Backup | Rust requested bytes | 688 | 333620 | 688 |
| Backup | Wrapped C boundary requested bytes | 1504 | 4197372 | 1504 |
| Backup | Unwrapped RSS, KiB | 3360 | 8268 | 3612 |
| Epoch | Rust requested bytes | 880 | 333812 | 880 |
| Epoch | Wrapped C boundary requested bytes | 134584 | 4330452 | 134584 |
| Epoch | Unwrapped RSS, KiB | 3884 | 8732 | 4076 |

Rust and wrapped C-boundary requested bytes return exactly to their warm
baselines. Native warm/final block counts remain five for
body/account/backup and thirteen for epoch, whose warmed entropy handle
remains live through its final observation. Wrapped C-boundary
observations are not SQLite-only attribution. The complete suite also
passed its unchanged quiet Rust allocation-counter and positive
controls; the SQLite observer itself enforces the whole-fixture peak and
exact warm teardown rather than a per-call zero-allocation interval.

Body and backup writable worker mappings were 249856 bytes; account and
epoch were 253952 bytes. Every mapping was equal before and after, with
the required adjacent inaccessible guard and no grow-down flag; explicit
worker joins precede completion. These are writable-region observations,
not measured frame high-water marks.

Selectors and the eight/nine/ten/thirteen observation counts remain
unchanged. Account verification, rollback/refusal/oversized and
unconsumed-source controls, permanent-ID retry, public backup, reopened
physical checks, source preservation and actual warmed entropy renewal
remain. Later account/source/destination identity checks use sequence
two; the successful body/account retry advances to sequence three. Each
store still owns eight readers plus one writer; reopened backup/epoch
source and destination stores retain eighteen native owners together.
Rust 2 MiB, wrapped C-boundary 17 MiB, sampled RSS growth 24 MiB and
guarded writable stack 256 KiB ceilings are unchanged. SQLite retains
separate 9 MiB per-allocation and 16 MiB process-wide caps across all
pools.

This qualifies the bounded public metadata PUT with retained
same-account/same-body inputs and sequential interleaved reads on one
process or guarded worker. It does not qualify parallel threads, every
write or deletion under portable resource observation, different
accounts/bodies, arbitrary-account or maximum-database work, numeric
frame peaks, transient RSS, complete faults, power loss or
whole-worker/service overlap. Requested-byte peaks and teardown describe
the combined fixture; they do not isolate allocations inside the
metadata commit. Earlier simultaneous-reader records remain tied to
their own artifact/source inputs and omit this metadata commit.

The artifact NAR was
a5cd9b76d1a104823ee12defcb1487e4b414d4ac0c6d88c8bb1feb3fca79e619.
Its BUILD-INPUTS records staged source NAR
1c5e6483b376184e6cf33eea1ba573f16ba5629ee9c68977605d8559dcc66aca
and unchanged vendor NAR
d122b8e7843f7da35cfb1f393dd0d8aea43ef534e9f5cffddc77a095fe971916.

The simultaneous-body-reader isolated release x86-64 musl qualification
on 2026-10-09 passed the complete portable command on its first attempt:
all eight static artifacts, API confinement, clean runtime, all existing
resource and positive controls, and the four guarded worker-stack cases.
Pinned Rust 1.96.0 and declared GNU inputs remain unchanged.

The earlier simultaneous-body-reader qualification retained eight
borrowed views and body inputs together during their initial postcommit
body verification. Fixed stack arrays retain all loans without new Rust
heap buffers. Every view matches the complete
account/epoch/sequence/floor identity; a ninth public capture returns
Busy. Progress interleaves one 64 KiB read from each input using the
existing single scratch buffer. Each input consumes all 512 chunks of
the same 32 MiB body, checks every byte and finishes its own digest. All
eight resulting pins pass cross-chunk and final-byte random reads. A
ninth capture still returns Busy while the eight pins and views remain
alive at the existing verified observation. After release, all eight
slots are reacquired together with the expected full identity, then
dropped before account verification or backup.

| Case | Observer | Warm baseline | Lifetime requested peak / maximum RSS sample | After teardown |
| --- | --- | ---: | ---: | ---: |
| Body | Rust requested bytes | 560 | 199846 | 560 |
| Body | Wrapped C boundary requested bytes | 1344 | 2943694 | 1344 |
| Body | Unwrapped RSS, KiB | 3340 | 6836 | 3572 |
| Account | Rust requested bytes | 624 | 199910 | 624 |
| Account | Wrapped C boundary requested bytes | 1424 | 2944014 | 1424 |
| Account | Unwrapped RSS, KiB | 3336 | 6836 | 3572 |
| Backup | Rust requested bytes | 688 | 333620 | 688 |
| Backup | Wrapped C boundary requested bytes | 1504 | 4197372 | 1504 |
| Backup | Unwrapped RSS, KiB | 3344 | 8252 | 3596 |
| Epoch | Rust requested bytes | 880 | 333812 | 880 |
| Epoch | Wrapped C boundary requested bytes | 134584 | 4330452 | 134584 |
| Epoch | Unwrapped RSS, KiB | 3872 | 8720 | 4064 |

The Rust quiet-allocation counter controls remain unchanged and passed.
Rust live requested bytes and wrapped C-boundary live bytes/block counts
return exactly to their warm baselines: five native blocks for
body/account/backup and thirteen for epoch, whose entropy handle remains
live through its final observation. Wrapped C-boundary observations are
not SQLite-only attribution. Body/account/backup before/after writable
mappings were 253952 bytes; epoch was 249856 bytes. Each pair was equal,
with the required adjacent inaccessible guard and no grow-down flag, and
the worker joined before completion.

The original eight/nine/ten/thirteen phase counts, selectors, fixed
dataset, rollback/refusal/ID-reuse controls, account verification,
public backup, reopened physical checks, actual warmed entropy renewal
and original source preservation remain. The pool configuration remains
eight readers plus one writer, with eighteen native owners in reopened
backup/epoch source/destination stores. Rust 2 MiB, wrapped C-boundary
17 MiB, sampled RSS growth 24 MiB and guarded writable stack 256 KiB
ceilings are unchanged. SQLite retains its 9 MiB per-allocation and 16
MiB process-wide caps across all pools. No production API, schema,
unsafe surface, dependency, reservation or native-cap change.

This qualifies simultaneous loan lifetime, interleaved progress and slot
reuse for this healthy same-body/same-account fixture on one process or
guarded worker. It does not qualify parallel threads, writes while
readers are retained, different accounts/bodies, arbitrary-account or
maximum-database work, numeric frame peaks, transient RSS, full
filesystem faults, power loss or whole-worker/service overlap. Earlier
maximum-pool records qualify owners with one borrowed view at a time and
remain tied to their own artifact/source hashes.

The artifact NAR was
9545c183d5b8f6dfbd4c6527fafc3464274e65631f1dbf4bacc9cd86573f5b2e.
Its BUILD-INPUTS records staged source NAR
a20f6c904c8f47129efd3a8fea6b7086b0308e4f50ca6943dc2160c21a8fd69e
and unchanged vendor NAR
d122b8e7843f7da35cfb1f393dd0d8aea43ef534e9f5cffddc77a095fe971916.

The maximum-reader-pool isolated x86-64 musl qualification on 2026-10-09
passed the complete portable command on its first attempt: all eight
static artifacts, API confinement and clean runtime, every original
resource/control case, and all four guarded worker-stack cases. Pinned
Rust 1.96.0 and the declared GNU tool inputs are unchanged.

The earlier maximum-pool-only portable SQLite body/account/backup/epoch
qualification requested the supported maximum eight readers plus one
writer at creation and every reopen. The source and destination stores
in backup/epoch overlap with eighteen native owners through the final
verification observations. The one-reader native warmup before baseline
is unchanged. This qualifies full owner pools for the fixed 32 MiB body
and mailbox dataset, not eight simultaneously borrowed views or
concurrent reader work.

| Case | Observer | Warm baseline | Lifetime requested peak / maximum RSS sample | After teardown |
| --- | --- | ---: | ---: | ---: |
| Body | Rust requested bytes | 560 | 199846 | 560 |
| Body | Wrapped C boundary requested bytes | 1344 | 2164550 | 1344 |
| Body | Unwrapped RSS, KiB | 3400 | 6060 | 3620 |
| Account | Rust requested bytes | 624 | 199910 | 624 |
| Account | Wrapped C boundary requested bytes | 1424 | 2164830 | 1424 |
| Account | Unwrapped RSS, KiB | 3396 | 6068 | 3624 |
| Backup | Rust requested bytes | 688 | 333620 | 688 |
| Backup | Wrapped C boundary requested bytes | 1504 | 4197372 | 1504 |
| Backup | Unwrapped RSS, KiB | 3396 | 8300 | 3644 |
| Epoch | Rust requested bytes | 880 | 333812 | 880 |
| Epoch | Wrapped C boundary requested bytes | 134584 | 4330452 | 134584 |
| Epoch | Unwrapped RSS, KiB | 3928 | 8768 | 4112 |

Native warm baseline and teardown block counts were five for
body/account/backup and thirteen for epoch, equal before and after. Rust
live requested bytes also return exactly to each warm baseline. The
epoch entropy handle remains live through its final observation. Wrapped
C-boundary observations include the instrumented native boundary; they
are not SQLite-only allocation attribution. Every before/after worker
mapping was 253952 bytes with the required adjacent inaccessible guard
and no grow-down flag; explicit joins precede completion.

All original phase sequences, body rollback/refusal/ID-reuse controls,
complete account checks, public backup, independently reopened physical
checks, actual warmed SystemEntropy renewal and source-preservation
assertions remain. Physical checks apply to backup/epoch; body/account
retain their original coverage. Existing Rust 2 MiB, wrapped C-boundary
17 MiB, sampled RSS growth 24 MiB and guarded writable stack 256 KiB
ceilings remain unchanged. SQLite retains its separate 9 MiB individual
and 16 MiB process-wide requested-heap caps across all pools. No
production API, schema, unsafe surface, dependency, native cap or
reservation changes.

This qualifies these healthy bounded fixtures and their owner
configuration. It does not establish arbitrary-account or maximum-
database behavior, simultaneous borrowed-reader/body execution, numeric
frame high-water, a transient RSS bound, full filesystem faults, power
loss, complete worker composition or service overlap. Earlier portable
records remain tied to their own artifact/source hashes and used two
readers plus a writer per measured store.

The artifact NAR was
b472f9efcb1dfa39ba924b083561182a8ece0487ad3b5bf825ac5a5bb65914b3.
Its BUILD-INPUTS records staged source NAR
0f661578dd2da5274d7d6cc094c2b3a0ea2c6504629483cd364564990b8df77b
and unchanged vendor NAR
d122b8e7843f7da35cfb1f393dd0d8aea43ef534e9f5cffddc77a095fe971916.

Separate --sqlite-body-stack, --sqlite-account-stack, --sqlite-backup-stack
and --sqlite-epoch-stack cases reuse the unwrapped artifact and existing
shared 32 MiB fixtures. Each fresh process starts one named worker with
a requested 240 KiB stack, then checks its actual writable mapping before
and after the complete scenario. The unchanged safe smaps helper requires
a region containing a stack marker, an adjacent inaccessible guard of at
least 4096 bytes, no grow-down flag and at most 262144 writable bytes.
The before/after sizes must match; all original fixture assertions, exact
phase counts and an explicit worker join precede the success marker.
These cases require the pinned release Linux x86-64 musl artifact and do
not emit allocation/RSS evidence. The runtime driver uses distinct logs
and fresh 300-second processes. Its exact parser rejects wrong scenarios,
missing/duplicated/reordered mappings, zero/overflow/oversized/changed sizes
and incomplete or extra completion output.

The complete isolated qualification on 2026-10-09 passed on its first
attempt, including all eight static artifacts, API confinement, all prior
allocation/RSS and runtime cases, and the four worker-stack cases. Pinned
Rust 1.96.0 and declared GNU inputs retained SQLite's unchanged 9 MiB
per-allocation and 16 MiB process-wide requested-heap caps. Observed
writable regions, equal before and after, were:

| Stack case | Writable bytes |
| --- | ---: |
| Body | 249856 |
| Account | 253952 |
| Backup | 253952 |
| Epoch | 253952 |

The epoch fixture warms actual SystemEntropy on that worker and
delegates exactly one 16-byte renewal fill. Backup/epoch retain physical
and complete account verification after reopening both roots and
original source checks; epoch mode changes only the destination epoch.
Body/account retain rollback, capacity and ID-reuse controls. This is
success for these fixtures on the fixed guarded region, not a numeric
frame peak, arbitrary-input/maximum-database, complete-worker,
allocation/RSS, full-fault, power-loss or activation claim.
The artifact NAR was
fa94cf77fb4451c79fbd567983fecd670003363781710cfb189d0a1fc51705f4.
Its BUILD-INPUTS records staged source NAR
d92fac421cd62e61b438f6c22e9886f5900ec0ee50b32dde4efb6a4e5686d1dc
and unchanged vendor NAR
d122b8e7843f7da35cfb1f393dd0d8aea43ef534e9f5cffddc77a095fe971916.

The isolated x86-64 musl backup qualification on 2026-10-09 passed the
complete portable command: all eight static artifact checks, API
confinement and clean runtime, including the old body/account modes and
new backup mode. It used pinned Rust 1.96.0, the declared GNU tools and
the current 9 MiB individual/16 MiB shared SQLite allocation ceilings.
The ten-point backup observations were:

| Observer | Warm baseline | Lifetime requested peak / maximum RSS sample | After teardown |
| --- | ---: | ---: | ---: |
| Rust requested bytes | 688 | 329972 | 688 |
| Wrapped C boundary requested bytes | 1504 (5 blocks) | 1988988 | 1504 (5 blocks) |
| Unwrapped RSS, KiB | 3444 | 5868 | 3664 |

Both allocation domains returned exactly to their warmed live baselines.
All Rust fields stayed unchanged from verified to account_verified;
wrapped C malloc calls increased by 5718. That interval includes release
of the earlier body pin/view, maintenance capture, the complete account
pass and view release. The copy interval includes destination validation,
checkpoint, explicit connection close, copy and publication; source and
restored intervals include reopen, physical integrity and account checks.
Caller roots and scratch remain live at backed_up. C-boundary counts may
include Rust System calls and are not disjoint memory or native-only
method attribution. Ten RSS samples do not establish a transient peak.
This qualifies the stated body/mailbox copy and dual-reopen fixture;
maximum database/account, power loss, full filesystem faults, guarded
stack and whole-service overlap remain separate.

The backup artifact NAR was
`631ad678414dcdf030b716063802092313257b166086fe09e5c0f689de39fdd9`.
Its BUILD-INPUTS records staged source NAR
`ed90427fbb877e31efc9db32de57847f9406e0e14a0ed8e7d1a847895a6e6a9b`
and unchanged vendor NAR
`d122b8e7843f7da35cfb1f393dd0d8aea43ef534e9f5cffddc77a095fe971916`.
An initial attempt failed to compile the Rust probe because a destination
closure captured both the live source store and its root for reopen.
The successful fresh build prepares both locked roots before creating
the store in one helper; the failed build supplies no measurements.

The fresh isolated x86-64 musl release qualification on 2026-10-09
passed the complete portable command, including the eight static artifact
checks, API confinement and clean runtime. It used pinned Rust 1.96.0,
the declared GNU tools, and the current 9 MiB individual and 16 MiB
shared SQLite allocation ceilings. The bounded account observations were:

| Observer | Warm baseline | Lifetime requested peak / maximum RSS sample | After teardown |
| --- | ---: | ---: | ---: |
| Rust requested bytes | 624 | 198086 | 624 |
| Wrapped C boundary requested bytes | 1424 (5 blocks) | 1060638 | 1424 (5 blocks) |
| Unwrapped RSS, KiB | 3360 | 4704 | 3508 |

All Rust counter fields remained unchanged from verified to
account_verified; the wrapped C malloc count increased by 5718. That
interval includes release of the earlier body pin/view, maintenance
capture, the complete account pass and view release. It is an observation
of this fixture and artifact, not a general zero-allocation guarantee.
Both allocation domains returned exactly to their warmed live baselines.
C-boundary counts may include Rust System calls and cannot be added to
Rust counts as disjoint memory. The nine RSS samples do not establish a
transient peak. This run qualifies the stated three-row account scenario;
arbitrary-account metadata, maximum-database maintenance, complete
filesystem faults, guarded stack and whole-service overlap remain separate.

The artifact NAR was
`1b2049d6d3f7006591571427ab929bf2495badd7cb67cc91cbf675452e614a05`.
Its BUILD-INPUTS records staged source NAR
`7589cfaed4e334ff4b622b321ce4fd9fb39233cd1538c118efba73e20c90f6f1`
and the unchanged vendor NAR
`d122b8e7843f7da35cfb1f393dd0d8aea43ef534e9f5cffddc77a095fe971916`.
An initial attempt refused an incorrect demand for positive ordinary
Rust allocations during the account interval. The successful fresh run
retained Rust positive forwarding controls and required positive wrapped
C-boundary account allocation; no artificial allocation was added.

The isolated x86-64 musl release qualification on 2026-10-07, using the
pinned Rust 1.96.0 kit and declared GNU tools on Linux 7.0.14, passed the
complete portable command including all eight artifact checks, API confinement
and clean-runtime cases. The 32 MiB body observations were:

| Observer | Warm baseline | Lifetime requested peak / maximum RSS sample | After teardown |
| --- | ---: | ---: | ---: |
| Rust requested bytes | 512 | 197926 | 512 |
| Wrapped C boundary requested bytes | 1296 (4 blocks) | 1060262 | 1296 (4 blocks) |
| Unwrapped RSS, KiB | 3364 | 4696 | 3504 |

Both allocation domains returned exactly to their warmed live baseline.
The C boundary includes allocator calls reached through the wrapped runtime;
it is not a SQLite-only attribution. The eight RSS samples do not establish
a transient peak. This run used the 2 MiB individual SQLite allocation cap
and does not qualify maximum-WAL checkpointing, guarded native stack,
filesystem fault recovery, backup or combined-service overlap.

The published artifact NAR was
`5fcb5ac60b44923eaa33a850aa6a86c82fab9544c82c13f93d15cec65bccd3f1`;
its BUILD-INPUTS records staged source NAR
`78b88ceb0dfb8dd4db99e22ed0bc13cea6bd5a367b4c63d90b8baeecd5e0ec58`
and vendor NAR
`d122b8e7843f7da35cfb1f393dd0d8aea43ef534e9f5cffddc77a095fe971916`.
The original runtime driver stopped after Rust evidence because the paired
probes reused a create-new log name. The successful run used separate Rust
and native command logs; it did not reuse a partially successful runtime.
