# Release source, builds and local installation

`td-update` is a dependency-free, source-built Rust program. The standard
image supplies it at `/bin/td-update`; the checkout's `./update` points there.
It does not change credentials. `./update` builds the checkout and requests
one installation through the paired authority; explicit `build` stops after
building. The root-only `apply-operation` helper stages the approved
source under root-private `/var/lib/td-update`, requires `td-boot
validate-source` to report exactly the approved ID, and calls `td-boot
on-volume install`, which resolves the
running kernel's UUID handoff inside the single writer, as specified in
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
Source remains the companion described by `td-install/DESIGN.md`, never
boot input or evidence authorizing an installation.

A newly created `~/src` has mode 0700, private to the human.
An existing parent keeps its permissions. Any existing destination, including
a symlink, is preserved. An initialized checkout belongs to the human, whose edits, branches, remote settings and
untracked files are never reset on boot. The private `~/.td-update/source.lock`
serializes cooperating initializers. Git inherits the same lease on stdin
and stderr so surviving Git workers prevent a restarted initializer from
reclaiming staging. Git diagnostics live in that private lock file; each
command clears them and resets the file offset under the lease.

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
recipe evaluator with the installed `/bin/cargo` and `/bin/rustc` launchers,
which fetch the running deployment's pinned Rust toolchain on first use.
`td-rust fetch` can prepare it ahead of time; subsequent compiler use and
Cargo compilation are offline. A failed download stops before a deployment
is built or activated. The compiler links through `/bin/cc`, td-link. It selects the native GNU target
explicitly and keeps control-plane build outputs under
`.td-build-cache/update-tools`. These locally compiled helpers retain
control-plane roles; this does not admit them as target recipe tools.

Before the evaluator runs, the installed `/bin/td-feed` fetches the
checkout's `net/Cargo.lock` closure with `warm crate-local net td-net`.
This breaks the cold-start dependency where a C compiler exists: native
`provision-net` rebuilds the fetch tool offline only after that verified
vendor closure exists. A td system has none for td-net's ring, so there
`provision-net` resolves to the installed multicall itself. A warm
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

The standard kernel enables the i386 ABI and `CONFIG_COMPAT_32BIT_TIME`.
td-jail's development syscall policy admits ordinary x86-64 and i386 code
(APPLICATIONS.md §C), and i386 programs that issue the 32-bit time
syscalls (older libcs, raw syscalls) need them for their time and futex
calls. Ordinary application confinement kills non-x86-64 syscall
architectures, and the independent target probe checks both unconfined
compatibility execution and confined rejection. The build host needs IA32
emulation too, because td-jail-test runs that probe there (DEVELOPMENT.md,
"Host kernel").

The new evaluator warms declared fixed-output inputs for `system-x86-64`,
then invokes `build-run` through the existing sandbox and recipe
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

Before requesting installation, the updater assembles a private deployment
under `.td-build-cache/update-deployments/`. Its build labels and
initramfs appendix follow `td-install/DESIGN.md`, "Installed startup menu".
It checks the recipe deployment against its manifest, copies its payloads,
adds the metadata appendix, and writes the replacement manifest. The
retained build result is also what `build` returns. Assemblies are named
by deployment ID and fully verified before reuse: repeated builds with the
same metadata reuse one copy without replacing a pending installation
source. Different deployment identities remain distinct retained outputs. The installed updater
owns this step; an older updater can still produce a legacy deployment
whose menu label has unknown dates. Source and build dates are UTC.

The default command and explicit `install` take that output's deployment,
hash its bounded regular manifest and invoke only installed
`/bin/td-authd request-update SOURCE ID`. The root intake independently pins
and checks the source before acknowledging it, and refuses while its
backoff runs after an unapproved request, which the client says. The user
then presses Ctrl+Alt+Escape and I and, after reviewing the full ID, types
the two-digit approval key the prompt shows; Enter does not install.
Escape or a wrong digit cancels before commitment. The installation state
machine and its uncertain-result policy are normative in
`td-authd/DESIGN.md`; no automatic retry or reboot follows.

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
files stable for the run and while retaining its overlay.

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

The guest uses its own source checkout, its own toolchain and local
caches. No host executable or store output is imported by this command.
An existing warm installation provides a bounded regression; a fresh
installation exercises upstream fetching and the cold build path. Reports
must identify which fixture was supplied rather than imply that a warm
pass rebuilt the entire recipe graph.

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
owner, operation, rollback/restart notice and the approval key's rows
against the actual 1280x800 framebuffer, reading the key, one of the 64,
off them. The expected text is independent of the compositor's
description builder; ASCII pixels are bound to its pinned font. The
preceding attention menu is checked against its separate small chrome
font before I is sent. The variable countdown and cursor are excluded.
Only after the full prompt matches does the oracle send Enter, which
must leave the same prompt for two seconds; then Escape. This first
request must end unsuccessful with both deployment selectors unchanged,
so an Enter that committed, whose prompt stays up while the helper
copies, fails here. It counted, and only a commit clears the count: by
the guest's clock, inside the 210 seconds from its admission, less a
ten-second margin, a direct `td-authd request-update` must be refused as
backing off, and a cancellation that outlasts that window fails as slow.
The oracle then waits the window out.

A second `./update` must request the same successor, which the oracle
installs by typing the key read off its prompt, sending no Enter.
Success must publish that successor as current and the initial
deployment as previous. A host test pins this order, since the oracle
needs an operator's installation. After an
explicit VM restart, boot health, kernel deployment ID, installed HELP
marker, user file and source edit must all agree. `result.txt`
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
recovery boots verify the user file and edited source.
The result records `automatic_rollback_verified=true` only after both pass.
Keep all three overlays with their backing files when retaining this evidence.

## Recovering an older runtime linker

An image with the runtime-linker recipe substitution bug rejects rustc's
`-B.../gcc-ld` argument: the linker's selected path contains the temporary
recipe build root instead of the account's compiler root. This fails during
checkout-tool compilation, before deployment installation. Pulling source
alone does not replace the already installed `/bin/cc`.

For that specific failure, an ordinary account can create a private temporary
alias for the erroneous root, then run the update that contains the fix:

```text
mkdir -m 700 /tmp/guix-build-td-link-runtime-0.1.drv-0 && ln -s "$(td-rust path)/lib" /tmp/guix-build-td-link-runtime-0.1.drv-0/lib && ./update
```

Proceed only if the directory creation succeeds; do not reuse an existing
path owned by someone else. The alias changes no installed file and needs
no root access. It lasts only until temporary files are cleared, so keep it
until the update finishes. The corrected deployment resolves its runtime
linker directly from the selected compiler and no longer needs this alias.
This workaround covers the observed linker failure; it does not establish
that later source preparation, building or deployment activation will pass.
