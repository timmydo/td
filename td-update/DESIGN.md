# Release source, builds and local installation

`td-update` is a dependency-free, source-built Rust program. The standard
image supplies it at `/bin/td-update`; the checkout's `./update` points there.
It does not change credentials. `./update` builds the checkout and requests
one installation through the paired authority; explicit `build` stops after
building. The root-only `apply-operation` helper signs with the retained
installation key and calls the single writer in `td-boot`, as specified in
`td-install/DESIGN.md`. Restart remains explicit.

## Initial checkout

The `release-source` oneshot runs `td-update init` through the existing
`td-login exec-as` human-account path after successful firstboot. Its process
belongs to the human session cgroup. It performs no network operation and
does not execute the bundled source. Console startup does not require it.
An ordinary boot fixture without the source companion simply has nothing
to initialize. Initialization requires the running system's mounted procfs
to establish the current UID without an additional syscall surface.

Initialization clones `/run/td-volume/td/source/repository.bundle` into
`~/src/td`, verifying its sole advertised HEAD against the adjacent canonical
40- or 64-character revision. It creates a local `main` branch and removes
the bootstrap bundle's `origin`: that local file is not an update remote.
The user configures a reachable Git origin before subsequent pulls. Source
remains the unsigned companion described by `td-install/DESIGN.md`, never
a boot trust root or evidence authorizing an installation.

A newly created `~/src` has mode 0700, matching the human workspace grant.
An existing parent keeps its permissions. Any existing destination, including
a symlink, is preserved. An initialized checkout belongs to the human, whose edits, branches, remote settings and
untracked files are never reset on boot. The private `~/.td-update/source.lock`
serializes cooperating initializers. Git inherits the same lease on stdin
and stderr so surviving Git workers prevent a restarted initializer from
reclaiming staging. Git diagnostics live in that private lock file; each
command clears them and resets the file offset under the lease. They contain
no signing key.

`~/src/.td-update-source.tmp` is reserved for unpublished initialization.
Interrupted staging can be removed only under the lease. If a destination
appears during cloning, initialization discards its unpublished staging.
Git settings and environment are cleared; hooks, templates, automatic maintenance, recursive
submodules and lazy fetches are disabled. Metadata output is bounded to
4096 bytes. History size follows Git and available disk space. The staged
tree is synced before rename and its parent afterward, with traversal bounded
to 200,000 entries and 64 levels. Publication rechecks destination absence;
rename itself refuses an existing nonempty directory. The human UID is
trusted to cooperate with initialization in its own reserved paths; this is
not a same-UID isolation boundary or an atomic no-replace syscall claim.

## Build command

From the checkout root, `./update build` compiles that checkout's builder and
recipe evaluator offline with the installed `/bin/cargo` and `/bin/rustc`.
It selects the native GNU target explicitly and keeps control-plane build
outputs under `.td-build-cache/update-tools`. These locally compiled helpers
retain control-plane roles; this does not admit them as target recipe tools.

Before the evaluator runs, the installed `/bin/td-feed` fetches the
checkout's `net/Cargo.lock` closure with `warm crate-local net td-net`.
This breaks the cold-start dependency: native `provision-net` rebuilds the
fetch tool offline only after that verified vendor closure exists. A warm
that reports success without complete inputs still fails the evaluator's
native vendor verification before any system build starts.

The native fetch-helper build keeps extracted dependency sources at a stable
content-addressed path under `.td-build-cache/native-vendor`. Each invocation
still copies and verifies the full archive set against `net/Cargo.lock`,
extracts private sources and computes their NAR digest. An existing cache
entry must match that freshly reconstructed tree, including names, bytes,
executable bits and symlink targets; no persisted marker authenticates it.
Corrupt entries are refused. Complete trees are published by rename and
retained across builds, so Cargo can reuse unchanged dependency compilation.
Cargo still decides invalidation for compiler, flags and checkout changes.
This is a cooperating same-user cache, not a same-UID isolation boundary.

The new evaluator warms declared fixed-output inputs for `system-x86-64`,
then invokes `build-run` through the existing sandbox and source-bootstrap
graph. It receives the exact builder just compiled through
`TD_BUILDER_SELF`. No host cache is imported, no application payload becomes
a compilation input, and the logical target prefix remains `/td/store`.
The first build therefore needs the graph's local build space and upstream
source downloads. No command pulls Git implicitly. `build` does not activate
the result. The evaluator's successful build must emit a complete bounded
`TD_RECIPE_RUN_OUT system-x86-64` receipt with an absolute output path;
logs stream to the terminal with a 64 KiB line ceiling. Missing or malformed
receipts, duplicate receipts and a failed process refuse installation.
Before each build phase, the updater names the work beginning on stderr:
checkout tools, their dependencies, system-source preparation (including
the fetch helper), and image construction. It names the deployment when
requesting installation. These messages report phase entry, not completion
or an estimate; the child process output and exit status remain authoritative.

The default command and explicit `install` take that output's deployment,
hash its bounded regular manifest and invoke only installed
`/bin/td-authd request-update SOURCE ID`. The root intake independently pins
and checks the source before acknowledging it. The user then presses
Ctrl+Alt+Escape, I and, after reviewing the full ID, Enter. Escape cancels
before commitment. The installation state machine and its uncertain-result
policy are normative in `td-authd/DESIGN.md`; no automatic retry or reboot
follows. Demo images without a retained matching key cannot install.

## Evidence

Pure tests cover absent source, preservation of existing and symlinked
destinations, malformed metadata, unsafe state and retained lock descriptors.
Host preflights additionally exercise real Git with SHA-1 and SHA-256 bundles,
interrupted staging, private edits, and corrupt or mismatched exports. The
host process fixture verifies helper argv, fresh builder identity and failure
propagation through compilation, fetch-tool dependencies, warm and build.
The compiler-only gate runs the binary's pure tests without host Git or
rustc. The target recipe checks its realized static executable, debug
companion and help path. The shared profiling roster includes its transitive Rust/LLVM
assembly boundary, alongside the standard glibc and libgcc boundaries.
Standard-image validation must prove the service actually initializes the
checkout as the human and preserves it on another boot; successful cloning
alone does not prove a complete system build or installation.
