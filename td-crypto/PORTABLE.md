# Portable build inputs

## Status

M03b2a implements header preparation; M03b2b prepares the remaining Rust and
GNU tool inputs. Isolated compilation and artifact qualification remain
M03b2c, and API confinement/TLS smoke remain M03b2d. This is a host build path for the
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
Every invocation reconstructs the expected kit from authenticated archives;
reuse compares the complete tree's NAR hash, including links, executable bits
and notices. A changed or symlink cache root fails. No persisted digest alone
authenticates a mutable tree. This prepares binaries and libraries as data;
it does not execute them or yet prove their runtime closure.

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
followed to import host files. Their resolution inside the future isolated
build remains part of M03b2c.

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
