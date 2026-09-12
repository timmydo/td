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
40- or 64-character revision. It removes the bootstrap bundle's `origin`:
that local file is not an update remote.
New bundles configure `https://github.com/timmydo/td.git` and branch `main`.
The publisher may override these with `--source-origin https://HOST/REPOSITORY`
and `--source-branch BRANCH`. The optional
`upstream` companion contains exactly three newline-terminated lines:
`td-source-upstream-v1`, the origin, and the branch. It is a regular file
of at most 4096 bytes. The shared parser accepts a credential-free HTTPS
origin and a plain Git branch name; helper URLs, local paths, embedded
credentials, query strings and fragments are refused. No ambient remote
or branch configuration is copied from the publisher.

With those settings, initialization creates that local branch at the
bundled commit, adds `origin`, and configures its remote and merge branch
without fetching. Otherwise it creates `main` with no remote. The human
can inspect `git remote -v` and use `git pull --ff-only` before `./update`;
divergent local work remains for the human to reconcile. Existing checkouts
retain their own settings even if the companion changes. An installation
without these settings needs a reachable origin configured before pulling.
Source remains the unsigned companion described by `td-install/DESIGN.md`, never
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

Packaged Cargo sources warm through td-feed's bounded native gzip and tar
metadata reader, shared with feed consumption. Preparation writes only
Cargo.lock and Cargo.toml, retains the original source archive for the
build, and requires no host tar or gzip executable. The package's shipped
lock still selects preparation; the build separately checks the recipe's
pinned archive and committed lock before compiling.
GNU long-name records are bounded to 4096 bytes and apply to exactly one
following ordinary GNU member. Orphaned or repeated name records, invalid
paths, duplicate selected metadata, link records and PAX extensions are
refused. Only the two selected metadata strings become filesystem writes;
archive paths are never extracted during this preparation.

Linux UAPI preparation uses `td-builder kernel-headers` with the recipe's
source path and SHA-256, x86 architecture, and a new private work directory.
Both accepted architectures emit identical headers under this pinned x86
policy; other architectures are refused. It supports Linux 4.14.67,
checking the eight upstream selection and transformation scripts before
proceeding. Rust handles xz/tar
extraction, header selection, syscall constants, textual sanitization and
canonical uncompressed GNU tar output. The installed C compiler builds the
pinned source's `unifdef.c` as a temporary control-plane helper. `HOSTCC`
selects one executable, defaulting to `gcc`; it is never parsed as a shell
command or split into arguments. The helper and its compiler never become
target recipe inputs through this operation.
No make, shell, sed, tar or xz executable participates in header preparation.
The generated i386 and x86_64 archives retain their existing seed digests.
A future selection-policy change requires updating this implementation.

The input archive is bounded to 512 MiB and verified before extraction;
the existing archive reader also bounds expanded bytes and entries.
Header traversal allows 32 levels, 2048 files, 2 MiB per header and 64 MiB
of header content. Output fixes order, permissions, timestamps, owners and
block padding independently of the working directory and umask. The source
cache's shared header lease travels on stdin through the builder, compiler
and sanitizer, so orphaned producers keep crash recovery from removing
active staging. Only a successful producer and synced temporary archive
can be renamed into the source cache. The target seed check remains the
authority for admitting the resulting data into the bootstrap graph.

The running kernel must support i386 execution: the source bootstrap builds
32-bit Mes and an early GNU toolchain before reaching native x86-64 tools.
The standard kernel enables that ABI. Application confinement still kills
non-x86-64 syscall architectures, with the independent target probe checking
both unconfined compatibility execution and confined rejection.

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


## Native release regression

`td-recipe-eval qemu-update --kernel FILE --selector FILE --disk FILE
--format raw|qcow2 --work NEW-DIR [--timeout SECONDS] [--accel tcg|kvm]`
runs the complete
update path on a disposable qcow2 overlay of a stopped private installation.
The supplied kernel and selector must match that installation. This is a
host-side QEMU oracle over operator-selected inputs, outside recipe gates;
it does not establish those inputs' source provenance. Keep the backing
files stable for the run and while retaining its overlay. A demo without a
retained signing key cannot satisfy this test.

The oracle uses four CPUs and 12 GiB of guest memory. TCG is the default;
`--accel kvm` selects hardware virtualization explicitly. KVM must be
available to the invoking process; QEMU failure is fatal, with no TCG
fallback. Each boot log and the completed result name the accelerator.
On a host where the account has just joined the kvm group, invoke through
`sg kvm -c 'COMMAND'` or a fresh login; querying `id USER` does not change
an existing process's group membership.

The new private work directory retains the overlay, serial logs and prompt captures. The
path must fit a Unix socket name: at most 70 bytes, with no comma. The
six-hour default deadline can be set explicitly between 60 seconds and
seven days. Every boot has an additional 30-minute health deadline.
Serial output is bounded to 256 MiB per boot and one MiB per line. A
64-line queue applies backpressure; teardown closes it before joining the
reader, so a blocked producer cannot prevent cleanup.

The guest uses its own source checkout, source-built compilers and local
caches. No host executable or store output is imported by this command.
An existing warm installation provides a bounded regression; a fresh
installation exercises upstream fetching and the cold build path. Reports
must identify which fixture was supplied rather than imply that a warm
pass rebuilt the entire bootstrap ladder.

On the first boot, the oracle waits up to ten minutes for the initializer
to publish the checkout's `update` symlink. This wait also respects the
overall deadline: boot health does not require source initialization.
Existing checkouts are immediately eligible; missing source companions or
failed initialization time out. The oracle does not retry initialization,
change service ordering, or reset existing source.

Inside the disposable overlay, the oracle changes the updater's HELP text
to a unique marker and leaves a unique user file. It runs `./update`,
waits for the installation request, uses QMP physical keys to open secure
attention and select I, then checks the complete expected deployment ID,
owner, operation, rollback/restart notice and confirmation choices against
the actual 1280x800 framebuffer. The expected text is independent of the
compositor's description builder; ASCII pixels are bound to its pinned
font. The preceding attention menu is checked against its separate small
chrome font before I is sent. The variable countdown and cursor are excluded. It cancels this
request and requires both deployment selectors to remain unchanged.

A second `./update` must request the same successor. Only after the full
prompt matches does the oracle send Enter. Success must publish that
successor as current and the initial deployment as previous. After an
explicit VM restart, boot health, kernel deployment ID, installed HELP
marker, user file, source edit and unchanged public signing identity must
all agree. The private key must remain unreadable to the human account.
This proves continued signing with the installation identity, not a
byte-for-byte comparison of the inaccessible private key. `result.txt`
exists only after the complete sequence succeeds. Normal restarts sync the guest, request QMP quit and require a successful
QEMU exit so its block driver can flush and close the overlay. This is a
host-directed restart, not a guest filesystem unmount. Failure teardown
kills only its owned child; source disks and selectors are never rewritten.

Add `--rollback yes` to exercise automatic recovery as part of the same
native release cycle. The default is `--rollback no`. After installation
and QEMU's successful exit, the oracle freezes the pending disk as
`pending.qcow2` and creates two children. `disk.qcow2` performs the healthy
successor boot above. `rollback.qcow2` exercises failed boots independently;
neither child modifies the frozen backing file, so acknowledging the healthy
branch cannot clear the failure branch's pending attempt state.

The failure branch uses the existing `td.boot-fail-target=1` fixture token.
Every configured attempt must select the successor, report the exact
remaining durable budget, establish read-only root and writable owned state,
then shut down with a successful QEMU exit before greeter or boot health.
Missing, duplicate, wrong-deployment or premature exhaustion evidence fails.
The next ordinary boot must report exhausted attempts and select the initial
deployment as previous, reach health and persist it as current. Another boot
must select it as current without consuming or exhausting an attempt. Both
recovery boots verify the user file, edited source and signing identity.
The result records `automatic_rollback_verified=true` only after both pass.
Keep all three overlays with their backing files when retaining this evidence.
