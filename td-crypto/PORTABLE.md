# Portable build inputs

## Status

M03b2a pins the musl notice source; M03b2b prepares the Rust inputs and the
Debian build root's host linker, binutils and host runtime. M03b2c supplies
isolated compilation and static artifact
qualification. M03b2d1 adds API confinement; M03b2d2 adds local TLS smoke.
This is a host build path for the
standalone td-mta executable; it does not grant the provenance of td's
target image graph. Nothing prepared here is automatically admitted
as a target recipe input.

## Musl notice source

The musl libc linked into both architectures' artifacts is the
self-contained `libc.a` of the pinned Rust target standard library below,
built by Rust from musl 1.2.5 (the version Rust 1.99.1's
`src/ci/docker/scripts/musl-toolchain.sh` names) with its upstream patches.
Nothing in the portable build compiles C, so no musl headers are prepared.
The upstream release is pinned only as the verified source of musl's
COPYRIGHT notice for that libc; it does not build or replace libc, and does
not claim that an unpatched musl 1.2.5 runtime is suitable for deployment.

| Input | Pin |
| --- | --- |
| Upstream archive | `https://musl.libc.org/releases/musl-1.2.5.tar.gz` |
| Archive bytes | `1080786` |
| SHA-256 | `a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4` |

Prepare a previously fetched archive offline:

```text
td-builder gate-crates crypto-musl-notice --archive /path/to/musl-1.2.5.tar.gz
```

The command prints the prepared directory under the worktree's
`.td-build-cache/crypto-musl-1.2.5-<output-digest>`. The archive argument
must name a regular file, not a symlink. It verifies a bounded in-memory copy
of the archive and passes those same bytes to td's existing gzip/tar reader. An
oversized archive fails the bounded read before hashing. It does not fetch,
run `configure` or execute `make`/`sed`.

Output contains exactly the upstream `COPYRIGHT` and a `SOURCE` receipt
naming the URL, archive hash and notice-only scope. Musl is predominantly
MIT-licensed; retain the complete upstream notice for its additional notices.
A fresh output is staged privately and published as one directory. The suffix
hashes both file paths and bytes (each relative path in ascending byte order,
then NUL, the decimal byte length, NUL and the bytes, concatenated into
SHA-256), so a changed receipt or license selects a new cache entry. Existing
output is re-read and compared with freshly verified source bytes. Modified,
missing, added or symlink files fail; the command does not silently repair
them. A concurrent identical publication may be reused. The host cache is
caller-owned, not a security boundary against another process with the same
uid. On a cache mismatch, remove only the named cache entry and retry. This
cache is reconstructible and publication is not fsynced. A hard-killed
preparer may leave a private `crypto-musl-notice-<pid>-<attempt>` scratch
directory; remove it only after confirming that preparer is no longer running.

## Rust host kit and target standard library

The portable path uses the upstream Rust 1.99.1 GNU host compiler, Cargo,
GNU host standard library and musl target standard library. The compiler and
its target library must be the matching upstream build: td's source-built
Rust identifies itself differently and rejects the upstream target library
with E0514. This host-only exception does not admit any downloaded Rust
component into td's source-built distribution. No rustup state is changed.

Each build selects four archives under the upstream prefix
`https://static.rust-lang.org/dist/2026-10-15/`:

| Archive (`.tar.xz`) | Bytes | SHA-256 |
| --- | ---: | --- |
| `rustc-1.99.1-x86_64-unknown-linux-gnu` | 84098960 | `2ff05ff9c08a90861e7f6e4065b40a8b87463b4576186fd613871c3e37f2091f` |
| `cargo-1.99.1-x86_64-unknown-linux-gnu` | 10954816 | `53f3521949404d595efb06a55c12c5658d4ed1c75566f33d3d514eaebe70bd8a` |
| `rust-std-1.99.1-x86_64-unknown-linux-gnu` | 31563228 | `109b074225bf5b9ea3820890876bfdf4ed161adbfaf94b2b4b5984c85ee5d958` |
| `rust-std-1.99.1-x86_64-unknown-linux-musl` | 41325128 | `1dd241eaeb19ff3b5180461c77bdee93d6cafc669eda9de834e7c06b0ec3ca95` |
| `rust-std-1.99.1-aarch64-unknown-linux-musl` | 31162764 | `2f3eb0d0610e2b7f6b2121d106e2490ac648decfa0c09b30873bdfc60de6c1ab` |

These hashes are from the Rust 1.99.1 upstream distribution manifest. Select
the first three host archives and one target musl standard library. Prepare
those four files and the musl archive in one directory, then run:

```text
td-builder gate-crates crypto-portable-prepare --archives /path/to/archives [--arch x86_64|aarch64]
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

The kit is published under `.td-build-cache/crypto-rust-1.99.1-<NAR-sha256>`.
The x86-64 build additionally requires NAR SHA-256
`7a54a59363f8c6168c6a7dad8be14ea357ce45e51e8d569a69e36737fd9ad962`;
the aarch64 kit requires
`984aaf7e17b2d9a4b2a5043e090c76e63abdda088d6b5ed2deec0a837af71210`.
This pins the reconstructed layout and notices as well as archive bytes.
Update this build pin only with a reviewed reconstruction/input change and
repeat the isolated artifact qualification.
Every invocation reconstructs the expected kit from authenticated archives;
reuse compares the complete tree's NAR hash, including links, executable bits
and notices. A changed or symlink cache root fails. No persisted digest alone
authenticates a mutable tree. This prepares binaries and libraries as data;
preparation alone does not execute them or prove their runtime closure.

## Debian build root: host linker, binutils and host runtime

The host linker for build scripts and proc macros, the binutils that list
symbols and split debug companions, and the host runtime the upstream Rust
tools run on are td's pinned Debian build root (AGENTS.md, "Debian build root";
`recipes/src/debian-root.lock`), the same tree td's C recipes build in. The
kit stays host-only: it is not a recipe and adds nothing to the target graph.
The preparer rebuilds or reuses a source-fingerprinted td-recipe-eval and
runs its `build-root-seed`, which places the root in the machine's shared
seed store from the pins `td-feed warm build-root` caches in `~/.td/sources`,
under the ladder lock a build takes, and prints the seed store, the seed db
and that lock's path. A cold pin cache fails there; warm it first. The kit
then holds the lock shared until its build ends, as a build does, so
`clear-store` waits rather than removing the tree under its mounts. That
answer only says where to look: td-builder takes the ladder lock
first, then authenticates the root against its own compiled
`debian-root.lock`, requiring the root's db to register exactly that
lock's store path at its pinned NAR hash, and re-hashes the tree on
disk. Anything else is refused; there is no
host or ambient fallback, and no caller-supplied compiler path selects a
tool. The preparer does not use `TD_RECIPE_EVAL`. Direct preparation has no
wall-clock deadline; its child has the existing parent-death handling.

The compile namespace binds three directories of the authenticated root,
read-only, each at its path in the root, and nothing else of it:

| Root directory | Mounted at | Role |
| --- | --- | --- |
| `usr` | `/usr` | Debian's GCC 14.2.0 for x86-64 (the host linker driver only) and binutils 2.44 for x86-64 and `aarch64-linux-gnu`; glibc 2.41, libgcc_s and zlib for the upstream Rust host tools, build scripts and proc macros |
| `usr/lib` | `/lib` | the usrmerge link the root's loader link and `libc.so` script name |
| `usr/lib64` | `/lib64` | the loader, `/lib64/ld-linux-x86-64.so.2` |

| Target | `nm`, `objdump`, `objcopy` |
| --- | --- |
| `x86_64` | `/usr/bin/nm`, `/usr/bin/objdump`, `/usr/bin/objcopy` |
| `aarch64` | `/usr/bin/aarch64-linux-gnu-nm`, `/usr/bin/aarch64-linux-gnu-objdump`, `/usr/bin/aarch64-linux-gnu-objcopy` |

No target C compiler, archiver or assembler is used: no admitted package
compiles C. Every `CC` and `AR` spelling Cargo's build scripts consult (plain,
both spellings of the target triple and of `x86_64-unknown-linux-gnu`, and
`HOST_CC`, `HOST_AR`, `TARGET_CC`, `TARGET_AR`) names the failing decoy, as
`CMAKE` and `PKG_CONFIG` do. td glibc is mounted nowhere, so nothing the
root's GCC links can resolve td's loader. The root's `/usr/bin` stays off
`PATH` (`/rust/bin` alone), since it also holds gcc, cmake and pkg-config:
every tool is named by absolute path. That guarantee is about lookup, not
presence: the root's compilers, cmake, pkg-config, perl, python3, glibc
headers and static archives are in the namespace, and a build script naming
one by absolute path would reach it. The
reviewed, policy-pinned dependency graph (`crypto_policy.rs`) is what
excludes that; the decoys prove only that no tool was found through
the environment. There is no
`LD_LIBRARY_PATH`: rustc, rustdoc, Cargo and rust-lld find the kit's own
libraries through their `$ORIGIN` run path and glibc, libgcc_s and zlib
through the root's loader. Build scripts and proc macros link through
`/td-crypto-host-linker`, which runs the root's `gcc -B/usr/bin/` against
the root's own glibc. The target link is unchanged: rust-lld with
`+crt-static` and the musl libc of Rust's self-contained target library.

Success prints `portable-rust`, `portable-musl-notice`, the root's
`build-root` store path and pinned NAR record, and its `portable-build-root`
directory. This is preparation output, not an installed artifact manifest.
No tool flags, API confinement, static-linking result or service capability
is established by this command alone.

All these caches are reconstructible and caller-owned. On mismatch remove
only the named cache entry and retry; there is no automatic repair or fsync
claim. A hard kill may leave `crypto-portable-<pid>-<attempt>` scratch behind;
remove it only after confirming that process has ended. The build host needs
space for archives, extraction and the reconstructed kit; the build root
lives once per machine in the shared seed store (about 510 MiB), and nothing
of it is copied into the worktree. Each extracted Rust component and its
private archive are removed after installation. Superseded content-addressed
kits remain until explicitly removed. Remove an old `crypto-*` cache entry
only when no preparation or build uses it and it is no longer needed; a later
preparation reconstructs it. Service memory budgets
do not describe build-time resource use. Both commands require an x86-64
build host; aarch64 is a cross-compilation target, not a supported build host.

## Aarch64 cross-compilation

```text
td-builder gate-crates crypto-portable-cross-build --archives /path/to/archives
```

This command uses the aarch64 kit and the build root's Debian
`aarch64-linux-gnu` binutils (`nm`, `objdump`, `objcopy`) in an isolated
offline build. It applies the same dependency admission, API confinement
and fallback refusal as the x86-64 path. Rust uses its generic ARM CPU
baseline and requests frame pointers and level-1 debug data. No C is
compiled, and the root carries no arm64 cross compiler, C library or
libgcc. Rustdoc's pinned baseline features are `neon` and
`outline-atomics`.

Rust's ARM target defaults to a non-PIE static executable. This build uses
stable `link-self-contained=no`, PIC and explicit linker flags to select the
pinned target kit's `rcrt1.o`, `crti.o`, `crtbeginS.o`, `crtendS.o` and `crtn.o`.
The musl static-PIE startup performs relocation; no runtime loader is needed.
The output must still pass the static-PIE check. Every host and target C
compiler and archiver selection points to the refusal decoy.

The output name is `crypto-artifact-aarch64-cross-<NAR-sha256>`, distinct
from the x86-64 runtime-qualified artifact. It contains `td-mta`, its
matching `td-mta.debug`, and seven test executables:
`td-crypto-smoke`, `td-mta-config-smoke`, `td-mta-format-smoke`,
`td-mta-transport-smoke`, `td-primitives-smoke`, `td-primitives-vectors`
and `td-primitives-ssh-vectors`. Every executable must be an aarch64 static
PIE;
x86-64 objects fail validation. The runtime/debug pairing, notices and input
receipt use the same checks and publication rules as the x86-64 artifact.

Cross-compilation does not execute ARM code. Its receipt explicitly records
`runtime not-run cross-compiled`, and the command's success message names
the separate emulated runtime below. It does not run the x86-64
clean-runtime process suite or allocation/RSS probes and cannot substitute
for that evidence.

### Emulated aarch64 runtime

```text
td-builder gate-crates crypto-portable-emulated-runtime --artifact .td-build-cache/crypto-artifact-aarch64-cross-<NAR-sha256>
```

This explicit host qualification runs a published cross-build's executables
under QEMU's user-mode emulator, the first `qemu-aarch64` on the caller's
`PATH`. Without one it refuses and runs nothing; it never skips a case. The
emulator is a host tool of this qualification only: it enters no build
namespace, no artifact and no receipt, and nothing it does reaches the
target graph. The argument must name a
`crypto-artifact-aarch64-cross-<NAR-sha256>` directory directly under the
worktree's `.td-build-cache` whose NAR hash still matches its name. The
command copies its eight executables and the debug companion into private
scratch, repeats the aarch64 static-PIE and debug-pair checks, and runs
them in a fresh namespace holding only those copies, the static td-builder
helper, the emulator at `/emulator/qemu-aarch64` and, for a dynamically
linked emulator, the libraries its own interpreter resolves for it
(`ld.so --list`), each bound read-only where the loader's spelling lands
inside the namespace: `.` and empty components dropped and each `..`
taking its parent, as the namespace's plain directories resolve it. A
spelling that is not absolute or climbs past the root refuses, and so does
one whose `..` leaves a symlinked directory on the host, where it would
reach another file than the one bound. Only the directories leading to a
bound file exist inside, so a spelling whose `..` climbs out of a
directory not on that path, such as `/a/b/../c/x.so`, names nothing there
and the emulator fails to start. The namespace holds no
`ld.so.cache` and no library directory beyond those paths, so an emulator
that resolves a library only through the cache at run time fails to start
rather than reaching the host's.
Static musl executables need no sysroot.

Inside, `td-mta --version` and every functional case the x86-64 clean
runtime names run by exact name, one emulated process each, with a
300-second deadline per case, the same 64 KiB output bound and the same
requirement of exactly one passing test: the 135 `td-crypto-smoke` cases,
the two `td-mta-format-smoke` provider-hash cases, the two
`td-mta-config-smoke` host scenario cases and the 64 non-ignored
`td-mta-transport-smoke` cases. Each prints `aarch64 emulated: PASS` or
`FAIL` with its executable and name; the first failure stops the run. The
three gateway process cases start the `td-crypto-smoke` TLS peer through
the same emulator (`TD_MTA_TEST_PEER_EMULATOR`, a test-only variable that
`crypto-cargo` clears, so no host test run inherits it from the caller). The ignored stack probes and the x86-only CLI,
allocation and RSS suites are not run: they measure x86-64 resources, and
the ARM build does not produce the last three.

The three td-primitives suites then run whole, with one test thread:
`td-primitives-smoke`, the crate's unit tests, `td-primitives-vectors`, its
independent known answers (`tests/vectors.rs`), and
`td-primitives-ssh-vectors`, those of the primitives td-ssh uses
(`tests/ssh_vectors.rs`). `tests/confinement.rs` is not built: it reads the
crate's source at run time. Each suite is asked to `--list` its tests and
its ignored ones and then run, three emulated processes with a 900-second
deadline each, and the run's one summary must report every listed test that
is not ignored as passed, none failed and none filtered out, so a suite
that runs fewer tests than it holds fails rather than passing on less. No
count is pinned here; td-primitives' suites state their own row counts. The
ignored test is the timing-leakage case, which no runtime runs (below).
Each prints `aarch64 emulated: PASS` with its counts and time. Last,
`hardware_backends_exercised` runs alone with its output shown, and its
three lines, which name each hardware backend and whether it ran, are
printed: all three are x86-64's, so here they say absent, and the suites
ran td-primitives' software paths only.

Emulation is functional evidence only. It translates aarch64 instructions
to the host's, so it says nothing of aarch64 timing, of a real core's memory
ordering, alignment or PSTATE.DIT behavior, or of a minimum kernel version,
and it leaves no resource evidence.

A run qualifies an artifact only when `td-mta --version`, all 203
named cases and the three td-primitives suites pass. The emulator's version
line is printed first; the commit that changes the artifact or this
qualification records its run.

These cases cover crypto, configuration, stored row formats and the
SMTP/TLS adapter; they do not launch the installed receiver. Follow DEPLOYMENT.md's
private pilot procedure for the executable's configuration, STARTTLS,
receiving and restart checks on a real ARM host before taking production
mail. The full td distribution's architecture and provenance claims are
unchanged.

## X86-64 isolated compilation and artifact

```text
td-builder gate-crates crypto-portable-build --archives /path/to/archives
```

This explicit qualification command first performs the same input preparation
and provisions the locked registry archives through td-feed. Preparation can
fetch declared fixed-output sources. Compilation runs without network access.
The ordinary Cargo preflight still qualifies the host build; it does not
silently claim to have run this separately provisioned portable command.

The driver stages manifests, locks, `src/` and optional `tests/` for
every pinned local source package (DESIGN.md names the twelve), plus the
three test-only
oracle files listed below. It refuses symlinks and special files. It
rechecks staged manifest/lock pins, reconstructs the verified vendor tree,
and mounts these inputs read-only. It uses the existing
source-fingerprinted static td-builder helper for namespace entry, host
linking and failing fallback decoys. This helper is host control plane,
not part of the installed binary. Build/output and Cargo home are private;
no previous Cargo target directory is reused. Caller-owned source/cache
trees must not be concurrently modified. No hostile same-uid writer
boundary is claimed.

The namespace has only these mounts plus the Rust kit and the
build root's `/usr`, `/lib` and `/lib64` listed above; the musl notice is
hashed and retained by the host, not mounted. It has private `/tmp`,
minimal `/dev`, private `/proc` and no host `/usr` or `/bin`, no td glibc, no
OpenSSL headers and no Cargo configuration. Cargo's
environment is cleared and reconstructed. The inner helper records each actual
Cargo command's arguments, working directory and explicit environment in
`BUILD-INPUTS`; these include frozen/offline source replacement, two jobs, epoch 0,
explicit Rust and host-linker paths and the C fallback decoys.
Every C compiler and archiver spelling, CMake and pkg-config point to a failing
td-builder applet which writes a marker; OpenSSL points to a nonexistent
directory. The positive build refuses any marker. A missing tool, archive or
library is an error, with no ambient substitute. There is no Zig dependency.

Rust links the target through its bundled rust-lld, static musl target std and
`+crt-static`, with `target-cpu=x86-64`.
Compiled Rust requests frame pointers and level-1 debug information;
release/bench profile stripping is explicitly disabled, and the artifact check
requires an ordinary symbol table in every executable but the split receiver.
Source/vendor/output paths map to `/td-build`, `/td-cargo/vendor` and
`/td-build-root`. Upstream prebuilt std/libc remain profiling boundaries. This host portable artifact is not the target
image's whole-closure profiling qualification.

The linker requests a deterministic SHA-1 GNU build ID. After symbol
inspection, the compiled constant-time check below runs on the unstripped
receiver; then the build root's `objcopy` creates `td-mta.debug` from the
receiver, prunes all DWARF, line tables included, using the shared target
profiling policy, then strips the runtime. The existing ELF pair validator
requires matching build IDs, ordinary companion symbols, no companion DWARF
and a stripped runtime. The host repeats that validation on its private
collected copy before running the process suite against the exact stripped
receiver. Both files ship
in the same content-addressed artifact. Probe executables retain embedded debug
data and never become installed service dependencies. This pair does not
establish frame-pointer coverage for the upstream prebuilt std/libc. [DEPLOYMENT.md](../td-mta/DEPLOYMENT.md) documents the installation
layout and receiver operations.

Cargo's selected normal/build graphs must match admission. Its artifact records
must select exactly one expected binary/test profile. All eleven results must be
x86-64 static PIEs with an executable entry point and no ELF interpreter,
DT_NEEDED or runtime search path. A second fresh namespace mounts only the
result and static test supervisor, then runs the installed name's version command
and each SHA-256, mail-format, PEM/identity/trust, entropy, provider-construction,
TLS, mail clock/TCP/TLS transport and configuration-stack smoke case in its own
process.
Before those component probes, a separate namespace mounts only `td-mta`
and the `td-mta-cli-smoke` test executable. The latter is the `operator_cli`
suite compiled with `TD_MTA_PORTABLE_CLI=1`: only this test binary selects
`/artifacts/td-mta` instead of Cargo's build path. Production code has no
environment override. No td helper is installed or mounted there;
host-sandbox's builder init remains PID 1 and is visible through `/proc`.
The outer test supervisor invokes host-sandbox and enforces a 120-second
deadline and a 64 KiB collected-output limit, requires the complete
three-test unfiltered passing summary, and fails on timeout. Namespace
teardown removes remaining processes. The ordinary host Cargo wrapper
clears the portable compile-time switch. This runtime fixture does not
apply the host test runner's data-memory cap.

The three process tests check the PostgreSQL serving help, refusal of
removed local-store commands, and required serving/control arguments
against the exact retained executable. They do not start a database or
qualify serving. The separate native PostgreSQL executable fixture checks
SMTP delivery, HTTPS device authentication and revocation, control stop,
restart and SIGTERM/SIGINT shutdown against a real database. That native
fixture is not a portable-runtime result. The runtime/debug companion pair
is qualified separately as described above.

The component-probe namespace has no compiler, root-data file, loader or
library mounts. Ordinary component commands have a 30-second deadline;
selected storage/resource probes below allow 300 seconds. The separate CLI
suite has its cumulative 120-second externally supervised deadline. Each
Cargo command has a 20-minute deadline. Parsed Cargo stdout is limited to
8 MiB (graphs to 256 KiB); each JSON record is limited to
256 KiB and 64 nesting levels. Logs on disk are temporary, not a streaming
output quota. M07 owns TLS allocation and entropy-failure qualification;
a Result wrapper cannot contain aborts.

After the compile namespace exits and its descendants are reaped, the host
requires an exact regular-file output inventory: eleven binaries and the inner
command record plus the receiver debug companion. It rejects output
directory/file symlinks and additional files, then copies these checked inputs
into a fresh private directory outside the compiler's writable mount. Only this directory receives host-written metadata
and notices, and only it is bound into the runtime fixture and published.
The internal commands require the host-sandbox marker and expected input paths
before writing. This guards accidental direct invocation, not callers forging
their environment; namespace entry remains the isolation boundary.

Success prints `.td-build-cache/crypto-artifact-<NAR-sha256>`, containing:

- `td-mta-rss-probe`: separate unwrapped process RSS observations, with no
  service use or inferred whole-service/transient-peak bound.
- `td-mta-rust-allocation-probe`: dedicated System-forwarding allocation
  qualification process; never an installed executable or service dependency.
- `td-mta`: the installed executable. Its configuration, `serve`, status
  and stop commands and remaining service limits are documented in
  td-mta/CONFIG.md and STORAGE.md; this artifact's process suite checks
  its arguments and does not qualify serving.
- `td-mta.debug`: the matching symbols, without line tables, mode 0644.
  Retain it with the executable; it is not a separate download.
- `td-mta-cli-smoke`: the three-test help and argument process suite above,
  selecting this exact `td-mta` under the separate fixture described above.
  It is a test controller, never a service dependency.
- `td-crypto-smoke`: separate qualification test executable, not a service
  dependency. It exercises the facade over td-primitives and td-tls-provider,
  compiled from their staged trees, including the known-answer tests over
  the committed AWS-LC fixtures (DESIGN.md); the clean runtime runs its 135
  named cases.
- `td-primitives-smoke`, `td-primitives-vectors` and
  `td-primitives-ssh-vectors`: td-primitives' unit tests (`--lib`) and its
  two known-answer suites (`--test vectors`, `--test ssh_vectors`), built
  by the same pinned toolchain with the same flags as `td-mta`, so the
  primitives' own suites run on the code this compiler makes of them. They
  are libtest builds of the crate, not the `td-mta` binary: inlining and
  layout may differ from the code linked there. The build stages
  td-source-scan, td-primitives' std-only dev-dependency, as a pinned local
  source, because Cargo compiles a crate's dev-dependencies for any of its
  test targets; none of these three uses it. The clean runtime runs each
  suite whole with one test thread and a 120-second deadline per process
  and requires every listed test that is not ignored to pass, as the
  emulated runtime does, then prints `hardware_backends_exercised`'s three
  lines: which of the AES-NI and PCLMULQDQ, AVX2 and SHA-NI paths this
  machine selected, and so which the suites ran. A build host without one
  of them runs that primitive's software path only, and its line says so.
  They are qualification executables, never service dependencies;
  `td-primitives-smoke` also carries the timing-leakage case below, which
  no runtime runs.
- `td-mta-config-smoke`: integration test linked to the production td-mta
  library for the bounded configuration stack checks in td-mta/CONFIG.md.
  Generic loader/materializer/writer functions compile in the test crate;
  this does not qualify a future installed service caller, even with the
  same reader type. It is not a service dependency. The same release Cargo
  graph builds it with `--test config_stack --no-run`; selection requires
  a test target and test profile. Runtime runs `host_loader_scenarios` and
  `host_materialized_scenarios` by exact name, then selects each ignored
  case by its exact name and requires one passed test plus one positive
  bounded mapping measurement. `portable_loader_stack` retains its 176 KiB ceiling and
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
  not establish heap, maximum-input/CPU-path coverage or service RSS.
  The same combined scenario set runs in an ordinary host test for fixture
  drift only; host execution provides no target stack bound.
  Stack probing in the prebuilt musl libc and Rust std objects is not
  qualified: such a frame could skip the guard. Successful execution and an
  observed mapping do not prove every overflow would fault. RESOURCES.md
  retains that admission obligation.
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
  generation budget, generation-bound IDs, queued/backend session ownership,
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
  musl, not full service admission, allocation/stack/RSS, or
  complete SMTP/STARTTLS protocol integration.
- `BUILD-INPUTS`: staged source, vendor, reconstructed Rust kit, musl notice
  (`musl-notice`) and helper NAR hashes, the build root's store path and pinned NAR hash
  (`build-root PATH sha256:...`), target and actual inner
  Cargo argument/environment records.
- `notices/`: upstream LICENSE/COPYING/COPYRIGHT/NOTICE/AUTHORS files, including nested
  provider attribution, Rust's bundled library notices and musl's COPYRIGHT
  (`notices/musl`) for the linked libc. Every package of
  the pinned active graph (`crypto_policy.rs`'s ACTIVE and POSTGRES_ACTIVE),
  build-only packages included, must contribute at least one notice. A
  locked-only package compiles into nothing here: its notices are retained
  when it ships them and not required when it does not. Their presence does
  not claim that all packages are linked.

The artifact hash covers binaries, receipts and notices. Each invocation rebuilds
from a fresh target directory; an identical result reuses an existing verified
artifact. Changed or symlink cache entries fail instead of being repaired.
Concurrent identical publication is allowed. No fsync durability is promised
for reconstructible build caches. Private `crypto-build-<pid>-<attempt>` trees
are removed on normal completion/error; after a hard kill remove one only after
confirming its process ended. Retained artifacts follow the same explicit cache
cleanup rule as prepared inputs.

## Compiled constant-time form

Both portable builds, x86-64 and aarch64, check the receiver they produce
before stripping it. The build root's `nm --defined-only --demangle
--print-size` (the target's own binutils, above) lists its text symbols,
and each td-primitives routine td-crypto/DESIGN.md names as branch-free
must appear under its exact demangled name:

- `<td_primitives::ec::Curve<4>>::add` and `<td_primitives::ec::Curve<6>>::add`,
  P-256's and P-384's complete point addition;
- `td_primitives::aes_gcm::sbox`, `ortho` and `shift_rows`, the bitsliced AES
  rounds;
- `<td_primitives::aes_gcm::HashKey>::absorb`, the integer GHASH.

td-primitives marks each `#[inline(never)]`, so the symbol is the one copy
of its body that the build runs. A name with no symbol fails the build, so
a rename, or an inlining that leaves no symbol behind, cannot silently drop
a routine from the check. Every copy under the name is checked. The
target's `objdump --disassemble --insn-width=16` disassembles exactly each
symbol's address range, whose instructions must run back to back from the
symbol's start to exactly its end, by the encodings objdump prints; a
listing that stops short, skips bytes or prints `(bad)`, `.word`, `.inst`
or another directive fails. The build fails on any conditional branch
there: x86's `jcc`, `jrcxz` and `loop` family, aarch64's `b.cond`,
`bc.cond`, `cbz`, `cbnz`, every other `cb` form (FEAT_CMPBR's `cb<cc>`,
should `target-cpu` be raised to emit it), `tbz` and `tbnz`. On aarch64,
`b` and `bl` are direct, `br` and `blr` indirect and `ret` a return; any
other branch form, the pointer-authenticated `braa`, `blraa`, `retaa` and
the rest included, and any other mnemonic beginning with `b` that is not
a known data-processing instruction, fails as unrecognised. On x86, the
prefixes the matcher reads past are those rustc and LLD emit. A call or
jump leaving the routine must name its target directly, and that target
must be another listed routine or the slice-generic
`td_primitives::bignum::mont_mul`, whose branches count its loops over
public lengths; an indirect one fails. Each routine's instruction and
outgoing-call counts reach the build log, and the commands reach
`BUILD-INPUTS`. Unit tests hold the matcher to synthetic disassembly of
both architectures, passing and failing.

The check is deliberately narrow. It reads the out-of-line bodies of these
routines and nothing else: an inlined copy would not be read, which is why
each is kept out of line. `Curve::add`'s fourteen Montgomery
multiplications are calls to `mont_mul`, whose body is not gated; the
side-channel review found it branches only on slice lengths. The check
does not follow any callee; it does not check the routines that branch
only on loop counters and lengths (the P-256 and P-384 table selection
inside `multiply_sum`, the X25519 ladder, ChaCha20's block, `ct::eq`,
`mont_mul`), because telling a counter from a secret needs data flow the
matcher does not have, and those remain reviewed rather than gated; and
it says nothing of memory access patterns or of instructions whose latency
depends on their data, a multiplication's or a division's included. It
holds the compiled form of what was reviewed against a later compiler,
flag or source change, which must then fail here or pass a renewed review.
The build log's counts for a run are recorded by the commit that changes
the check or the routines.

## Timing-leakage run

```text
taskset -c 7 td-builder gate-crates crypto-portable-leakage --artifact .td-build-cache/crypto-artifact-<NAR-sha256>
```

This on-demand measurement, never run by a gate or `ready`, takes a
published x86-64 artifact, which must be a `crypto-artifact-<NAR-sha256>`
directory directly under the worktree's `.td-build-cache` whose NAR hash
still matches its name (an aarch64 artifact is refused). That check
proves the directory unchanged since it was published, not which tree
built it, so the command first prints the artifact's path and the staged
source hash its `BUILD-INPUTS` records, and a recorded run cites both. It
copies `td-primitives-smoke` and `td-crypto-smoke` into private scratch,
repeats their static-PIE checks, and runs the two ignored timing-leakage
cases, td-primitives' `leakage_tests::timing_leakage` and td-crypto's
`pem::tests::pem_timing_leakage`, one process each with one test thread
and a cleared environment that keeps only `TD_LEAKAGE_SAMPLES`. Their
output streams to the caller; each, selected by `--exact`, must report a
passing test. So what is measured is the code the pinned toolchain makes
with the portable flags, in these libtest executables rather than the
`td-mta` binary, whose inlining and layout may differ. The cases inherit
the caller's CPU affinity, so `taskset` pins them. They run without a
namespace of their own: they are td's own test executables from pinned
sources, as the host test builds the command replaced were, and a
namespace would neither change their timing source (`Instant`, through
the vDSO) nor add a boundary the measurement needs.
td-primitives/DESIGN.md ("Timing measurement") says what is measured and
records results, and DEVELOPMENT.md gives the procedure.

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
`a` bytes. Differential cases compare against td-primitives' separate SHA-256
(CPU-dispatched to SHA-NI where present) at every byte length through 257 and selected larger block
boundaries through 4097, with varied chunks and every split through 257.
Separate cases verify terminal refusal, retained-state clearing, bounded inline
storage and redacted Debug. The length limit uses a synthetic counter near its
ceiling; synthetic differences exercise upper padding-length bytes without
hashing exabytes.

Direct digests have no provider unwind path, heap allocation or backend
calls. Source and exact-artifact control/address and call-graph review qualify
that narrow primitive; retain compiler/flags/target and artifact identity in the landing
record. This is not a guarantee for arbitrary compiler changes, other CPU
paths, secure erasure of all copies, or whole-service resource use. The
backend adapters still require whole-graph `panic=unwind`; their synthetic
failures do not simulate abort/OOM or entropy failure. DESIGN.md owns those
limits.

## Entropy qualification

Three exact cases in `td-crypto-smoke` exercise local bounded random fills,
nonempty constructor initialization, and synthetic returned errors after a
partial write. They verify that failed construction yields no handle and a
returned fill error clears the whole caller slice while preserving guards.
The local sample comparison catches no-op/constant-output wiring; it is not
an entropy-quality test. The host compile-fail doctests separately pin the handle's
Send and Sync refusals. No new portable executable is added.

These cases do not induce a kernel random-device failure or prove recovery
from abort/OOM or a bounded wait for the kernel's generator. Remaining
allocation/stack/RSS qualification follows DESIGN.md. No service is enabled by this probe.

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
induce random-device failure; allocation, blocking, stack/RSS and timing
qualification remain M07 obligations. DESIGN.md owns the accepted key formats,
secret lifecycle, output/retirement contract and failure limits.

Five `backend_tests` cases are fixed known answers from the committed
fixtures AWS-LC wrote while it was the backend: its four generated P-256
keys load here in all four accepted PKCS#8 shapes to its public points, and
its signatures verify under the independent oracle; four keys in this
backend's generated shape, which AWS-LC loaded and whose signatures it
verified when written, load to the same points; AWS-LC's signatures and
td-tls-provider's big-integer rows are taken by exactly their own policy
verifiers; AWS-LC's twenty certificate paths verify under the policy and a
changed signature does not; and the loader refuses out-of-range scalars and
mismatched embedded points.

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
leaf with td-primitives' P-256 signing. A small fixture DER encoder builds
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
classical groups and suites. Handshakes signed with RSA, P-384, P-521 and
Ed25519 keys run in the crypto-cargo leg's `handshake_peer_tests`
(DESIGN.md), not in this portable smoke; alternative CPU paths are not
exercised.
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
fallback decoys and Rust target flags as the shipping binary. This ordinary
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

A second compiler pass marks all four direct dependencies (rustls,
td-primitives, td-tls-provider and webpki-roots) private
using `--extern priv:NAME` and forbids `exported_private_dependencies`. This
also catches transitive foreign types and methods/associated types involving
`dyn` local traits that pinned Rust 1.99.1 rustdoc omits from its JSON. The
compiler pass is mandatory; the graph alone is insufficient. Both the
privacy pass and rustdoc use crate-scoped `RUSTC_BOOTSTRAP=td_crypto` for
diagnostic options only.
The shipping binaries have already been compiled and copied; their build uses
no unstable options. No nightly toolchain, Rust parser dependency or source-text
re-expansion is used. Cargo supplies the crate environment and dependency
resolution. Actual commands/environment and graph/fixture counts enter
BUILD-INPUTS.

Pinned rustdoc produces schema-61 JSON including private and hidden items. The
std-only walker follows public modules/re-exports, aliases, fields, variants,
functions, methods, generic bounds, traits and associated types. It follows
trait implementations attached to reachable local types, including private
traits. This deliberately rejects private-trait backend types on public
wrappers; private backend fields remain allowed. Every referenced type must
belong to td-crypto or std/core/alloc. Missing IDs, unknown public item kinds,
exported macros, changed schema or incomplete output fail.

The JSON target triple and enabled CPU features must match baseline x86-64
musl. Schema 61 omits `crt-static` from its CPU-feature list, so a generated
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
TLS sessions or allocation/stack/RSS admission.

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
mapping-drift fixtures, plus the committed ML-DSA-44 path AWS-LC wrote (and
verified with its defaults when it was the backend), which the selected
certificate policy refuses. Backend TLS fixtures use the selected provider.
Additional bounded local handshakes cover each classical group under TLS
1.2/1.3 and the six TLS 1.3/ECDSA TLS 1.2 suites. A peer offering only a test
group named X25519MLKEM768 fails in either direction (`NoKxGroupsInCommon`),
and a policy client refuses a server whose only key signs ML-DSA-44
(`NoSignatureSchemesInCommon`). td-primitives implements neither algorithm,
so no positive hybrid or ML-DSA baseline runs. RSA suites and P-384/P-521/Ed25519
signing schemes receive inventory coverage only. These checks qualify the algorithm component; they do not
claim public configurations, session limits or service admission.

## Owned TLS signing qualification

The portable artifact runs signature DER boundary and hash-once verification,
shared-key transform-error/unwind retirement, retained-identity lifecycle and
local TLS 1.2/1.3 handshakes through the owned signer. Both configurations
refuse after shared-key retirement; preceding peer name/protocol refusals
leave it usable. Resumption is disabled in this fixture. Private state faults
are synthetic Rust failures, not random-device/OOM/abort simulation. TLS roles,
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
verifiers and backend connections remain subject to M07e allocation/stack/RSS
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
The backend's deframer
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
remote peer, through a test wrapper around td-tls-provider's TLS 1.2
AES-256-GCM, which itself refuses extraction; facade configurations still
disable extraction. Separate cases
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
their existing allocator. The build uses the build root's nm to require the
`TD_MTA_ALLOCATION_COUNTERS` and `rust_alloc_probe` symbol-name substrings
in this test and refuse these sentinels in every other retained executable.
These are checks for the named probe, not exhaustive classification of all
allocator machinery; source confinement enforces its import boundary. The artifact inventory, static ELF checks and runtime
namespace include this executable explicitly. No common compiler/linker flag
changes. The runtime runs each mode in a fresh process. The default requires
its exact success line after model, forwarding and digest/SMTP checks. UNSAFE.md T1 and
td-mta/RESOURCES.md define the scope: requested Rust bytes only; libc's
internal allocations are outside it, and no RSS, whole-service or
allocation-elimination guarantee is inferred.

## Libc-boundary and foreign-symbol checks

No libc-boundary allocation diagnostic is built (UNSAFE.md T2 is retired);
td-mta/RESOURCES.md, "Libc-boundary observation", says why and records the
current run's stack-mapping, requested-byte and RSS results. The build
lists each retained executable's defined symbols with the build root's
`nm` and refuses any `__wrap_` symbol, any symbol starting `aws_lc_`,
`OPENSSL_`, `BORINGSSL_` or `ring_core_` (the native backends' prefixed
C symbols) and any mangled path naming the `aws_lc_rs` or `aws_lc_sys`
crates. This refuses those backends by name; it does not by itself prove
that every cryptographic routine is td-primitives.

The Rust allocation executable also runs `--tls-clients` in a fresh
process. The runtime validates all twelve ordered snapshot rows and their
completion records before printing observations. This exercises representative
public-root outbound policy generations and pending client construction through
the mail facade, without sockets. RESOURCES.md in td-mta defines columns,
positive controls and the limits of those diagnostic observations; they do not
qualify TLS session ceilings or whole-service RSS.

A separate `--tls-handshake` process additionally qualifies
the local TLS 1.3 observation path. It generates synthetic material, completes
an admitted loopback pair, transfers verified 16 KiB records in both directions,
and requires stable retained requested bytes across repeated transfers. All
thirteen ordered rows are validated before logging. These representative
requested allocation observations do not establish TLS memory ceilings or RSS bounds.

It additionally runs `--entropy-workers` in a fresh process. Seven
ordered rows record startup of eight test workers, cold RNG work on one then
all workers, repeated warmed fills, explicit joins including TLS destructors,
and final scope teardown. Warm fills must leave counters unchanged. Scenario
logs are distinct and completion records are exact. Requested
bytes/blocks can remain after all workers exit. These diagnostics do not
measure worker stack mappings or RSS and do not change service admission.

Fresh `--tls-fragments` processes additionally exercise the socket-free facade's
existing malformed-handshake reassembly boundaries with 16384-byte and 4096-byte
fragments. Twelve ordered rows distinguish setup, retained incomplete input,
decoding/capacity refusals, repeated cycles and teardown. The pending snapshot
precedes the final record and does not isolate a transient peak. All rows and
completion markers must match their scenario schema before logging.
This is an untrusted-input diagnostic, not a complete TLS/session/RSS bound.

The fresh `--tls-large-chain` processes reuse the local authenticated TLS 1.3
scenario with a signed leaf/intermediate/root chain above 60 KiB of PEM but
within the local loader's 64 KiB limit. Each padded certificate remains within
16 KiB DER. Thirteen ordered observations and exact completion use a separate
large-chain schema and log. Both endpoints and retained fixture
material are included; successful large-chain traffic is not a total service,
maximum-profile or RSS qualification.

Handshake and large-chain allocation records use schema v2 with separate
client, remaining-server and returned-wire-buffer release checkpoints. The
counter probe requires exactly 73748 requested bytes to disappear when the
four returned buffers drop. Other allocation scenarios retain v1. Endpoint release deltas exclude
shared state and stack storage.

Fresh `--tls-generations` processes compile sixteen
large files profiles into direct SMTP, HTTPS/MTA-STS and relay policies, retain
old and replacement owners, refuse a third generation, and repeat replacement
after releasing the old lease. Eleven ordered rows and a distinct v1 completion
are required. Counter refusal must be unchanged and repeated replacements must
not retain requested bytes. This is a profile-count fixture,
not a complete generation-memory bound; td-mta/RESOURCES.md defines its scope.

## Mail sampled RSS diagnostic artifact

`td-mta-rss-probe` is a separate integration executable with its own main.
It uses the ordinary allocator and no allocation-counter storage. Symbol
checks require the `rss_probe` sentinel only in this artifact and refuse the
Rust probe sentinels there. The exact artifact inventory,
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

The existing unwrapped mail test executable additionally controls six local
process fixtures: TLS 1.2, TLS 1.3 with two tickets, and TLS 1.3 with one large
ticket, each with a fresh Rust or RSS observer. A private test-only
crypto server reads four synthetic DER objects whose total exceeds 63 KiB and
fits 65000 bytes, independently of the local PEM loader ceiling. TLS 1.3 emits
either two counted opaque 16000-byte test tickets or one 65000-byte ticket;
the private ticketer refuses resumption and performs no encryption. Controller
and peer allocations are outside each observer. Explicit paths select only the existing qualification
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

The Rust allocation executable and the unwrapped RSS executable additionally
run `--tls-generation-routing` in fresh processes. It combines sixteen profiles,
sixteen listeners and 256 MTA-STS domains, including 255 long derived names,
while retaining two generations and repeating replacement. Every local chain
still obeys the PEM/DER ceilings. The runtime requires distinct ordered
`generation-routing` records and completion markers and rejects evidence from
the smaller generation case. No new executable, unsafe surface or link flag
is introduced. RESOURCES.md defines retained fixture attribution, checked
release/refusal invariants and the remaining aggregate-admission limits.

## Gateway trust generation observations

The Rust allocation executable and the unwrapped RSS executable additionally
run `--tls-generation-trust` in fresh processes. It constructs sixteen admitted
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

Fresh `--tls-certificate-list` modes in the existing Rust allocation and RSS
artifacts send an unexpected plaintext Certificate message in the TLS 1.2
format before ServerHello. Its 21800 empty DER entries fit in a bounded
65403-byte body, delivered as either 16 KiB or 4 KiB record fragments. The
pinned backend decodes the list before refusing the unexpected message. No
certificate or peer is authenticated.

Eleven exact `certificate-list` phase records and allocation v1/RSS v2
completions distinguish these observations from malformed ServerHello tests.
The allocation probe requires the first final-record step to exercise at least
512 KiB of additional requested-byte peak above its pending peak snapshot;
this qualifies the current pinned decoder path and is not a desired minimum
for a future implementation. It relies on the pin's vector growth and spare
capacity. Repeated refusals must retain no extra requested bytes. Terminal
failure must discard output and refuse further operations.
No executable, unsafe surface or link flag is introduced. RESOURCES.md
defines the attribution limits and remaining admission work.

## Mail database qualification

The mail artifact includes the pinned PostgreSQL client without a Tokio
runtime. Removed embedded-database allocation, RSS, worker-stack and backup
scenarios are not qualification of PostgreSQL. Existing crypto, TLS, MIME,
configuration and operator-CLI portable checks retain their stated scope.
The native gate separately requires real PostgreSQL client and service
fixtures; see DEVELOPMENT.md and td-mta/CONFORMANCE.md.
