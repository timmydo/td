# Portable build inputs

## Status

M03b2a implements header preparation; M03b2b prepares the remaining Rust and
GNU tool inputs. M03b2c supplies isolated compilation and static artifact
qualification. M03b2d1 adds API confinement; TLS smoke remains M03b2d2.
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

The driver stages only both crates' manifests, locks, `src/` and optional
`tests/`, refusing symlinks and special files. It rechecks staged manifest/lock
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
must select exactly one expected binary/test profile. Both results must be
x86-64 static PIEs with an executable entry point and no ELF interpreter,
DT_NEEDED or runtime search path. A second fresh namespace mounts only the
result and static test supervisor, then runs the installed name's version command
and each native SHA-256/provider-construction test in its own process. It has no
compiler, root-data file, loader or library mounts. Each runtime command has a
30-second deadline; each Cargo command has a 20-minute deadline. Parsed Cargo
stdout is limited to 8 MiB (graphs to 256 KiB); each JSON record is limited to
256 KiB and 64 nesting levels. Logs on disk are temporary, not a streaming
output quota. Native/TLS allocation, entropy failure and handshake qualification
remain M03b2d2/M07; a Result wrapper cannot contain provider aborts.

After the compile namespace exits and its descendants are reaped, the host
requires an exact regular-file output inventory: two binaries and the inner
command record. It rejects output directory/file symlinks and additional files,
then copies these checked inputs into a fresh private directory outside the
compiler's writable mount. Only this directory receives host-written metadata
and notices, and only it is bound into the runtime fixture and published.
The internal commands require the host-sandbox marker and expected input paths
before writing. This guards accidental direct invocation, not callers forging
their environment; namespace entry remains the isolation boundary.

Success prints `.td-build-cache/crypto-artifact-<NAR-sha256>`, containing:

- `td-mta`: the installed executable name, currently only `--version`/`--help`;
  all service arguments fail. It does not yet serve mail.
- `td-crypto-smoke`: separate qualification test executable, not a service
  dependency. It exercises native code that the current packaging entry point
  does not yet retain through a service caller.
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
