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

The driver stages both crates' manifests, locks, `src/` and optional `tests/`,
plus the three test-only oracle files listed below. It refuses symlinks and
special files. It rechecks staged manifest/lock
pins, reconstructs the verified vendor tree, and mounts these inputs read-only.
It uses the existing source-fingerprinted static td-builder helper for namespace
entry, host linking and failing fallback decoys. This helper is host control
plane, not part of the installed binary. Build/output and Cargo home are private;
no previous Cargo target directory is reused. Caller-owned source/cache trees
must not be concurrently modified. No hostile same-uid writer boundary is claimed.

The namespace has only these mounts plus the Rust kit, musl headers and five
native roles listed above. It has private `/tmp`, minimal `/dev`, private `/proc`
and no host `/usr`, `/bin`, OpenSSL installation or Cargo configuration. Cargo's
environment is cleared and reconstructed. The inner helper records each actual
Cargo command's arguments, working directory and explicit environment in
`BUILD-INPUTS`; these include frozen/offline source replacement, two jobs, epoch 0,
explicit Rust/C/ar/linker paths and all five AWS-LC source-build controls.
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
td-secret/src/fido_p256.rs and td-secret/tests/p256_vectors.txt. Stage these
regular files at their original relative paths and include their bytes in the
source digest. Missing files, symlinks and non-directory ancestors refuse the
build. No other engine/td-secret file is staged and no Cargo dependency is
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
existing independently calculated fixture digests with real SHA-256 over
FORMAT/CURRENT/table/journal headers, record/frame/manifest footers, whole-file
CURRENT-to-manifest and manifest-to-table/history bindings, blob `abc`, and
both import snapshot preimages. Updates use 1/31/64/65-byte chunks; changed
first/middle/last preimage bytes produce different digests. Frame footer input
includes the header digest and end magic; CURRENT input includes the selected
manifest footer. No fixture is regenerated by the code under test.

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
resident increase after touching a 16 MiB allocation. Six independent fresh
processes run the client, local handshake, entropy worker, fragmented
handshake, large-chain and sixteen-profile generation scenarios. The runtime
requires exact ordered rows, positive decimal KiB values and the scenario
completion record before logging.
Every mode has its own bounded log and 30-second deadline. Missing procfs
support or malformed/truncated rollup output fails qualification.

RESOURCES.md in td-mta defines the observation scope. These are sampled whole
fixture process values, including stacks and observer state. They do not
establish transient peaks, isolate provider costs or qualify service admission.

RSS completion records use v2 for every scenario, with the same additional
handshake release phases. The runner rejects earlier completion versions and
missing/reordered phases. An RSS delta is not required to match a requested
allocation delta.
