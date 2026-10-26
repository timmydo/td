# AGENTS.md — td

You are one of possibly several agents building a functional Linux
distribution. This file is the always-loaded contract: keep it short, put
component detail in the normative documents named below, and state a rule in
one place.

# What to read

Read this file completely. Load additional instructions only when the task
needs them:

- Before changing files, read `DEVELOPMENT.md` completely. It carries the
  worktree, commit, review, ready, and submission workflow.
- To find or bump outdated pins (sources, the Rust toolchain, the Debian
  build root, Cargo locks), read `UPDATING.md`.
- For application packaging or confinement, read `APPLICATIONS.md`.
- Before changing or adding `unsafe`, read `UNSAFE.md` and the touched crate's
  normative document.
- For login or credentials, read `td-login/THREAT-MODEL.md`.
- For mail serving, storage, or submission, read `td-mta/DESIGN.md`.
- For shared MIME parsing or decoding, read `td-mime/DESIGN.md`.
- For shared HTML rendering or text extraction, read `td-html/DESIGN.md`.
- For td-crypto or its consumer boundary, read `td-crypto/DESIGN.md`.
- For the shared FIDO2 client or SHA-256, HMAC and HKDF, read
  `td-fido/DESIGN.md`.
- For shared header lexical syntax, read `td-header/DESIGN.md`.
- For shared Unicode normalization, read `td-nfc/DESIGN.md`.
- For shared Markdown parsing, read `td-markdown/DESIGN.md`.
- For shared JPEG decoding or encoding, read `td-jpeg/DESIGN.md`.
- For shared PNG decoding or encoding, read `td-png/DESIGN.md`.
- For making or checking Btrfs volumes (`mkfs.btrfs`), read
  `td-btrfs/DESIGN.md`.
- For the Rust link driver td's Rust recipes link through, read
  `td-link/DESIGN.md`.
- For disk encryption or session unlock, read `td-install/ENCRYPTION.md`.
- For compositor/UI, service supervision, or installation, read the matching
  `td-compositor/DESIGN.md`, `td-svc/DESIGN.md`, or `td-install/DESIGN.md`;
  for the shared UI toolkit td-owned graphical programs depend on, read
  `td-ui/DESIGN.md`; for the terminal, read `td-term/DESIGN.md`; for the
  agent harness, read `td-agent/DESIGN.md`.
- Before changing target compiler flags, ELF debug handling, profiler code, or
  profiler image integration, read `td-profiler/DESIGN.md`.

Those documents are normative. If code changes an invariant they state,
amend the corresponding document in the same landing.

# Request scope

- For requests to answer, explain, review, audit, diagnose, or plan: inspect
  the relevant material and report the result. Do not edit, commit, push, or
  otherwise change state unless the request also asks for a change.
- For requests to change, build, or fix: complete the requested in-scope
  change, validate it, carry it through the review and ready workflow, and push
  its branch to `origin`. The pushed branch is the submission. Stop at a local
  branch only when the user explicitly asks for local or draft work.

# Build and trust model

td has two build graphs. Do not confuse the host control plane with the
target distribution artifact graph.

## Trust zones

td has three trust zones. Zone one is the target artifact graph, built from
pinned sources by the Debian build root's and the upstream Rust toolchain's
compilers, with imported Debian glibc and libgcc. Zone two is the host control
plane that evaluates and builds that graph but never enters it. Zone three is
each marked foreign prebuilt
application payload. A zone-three payload may live in `/td/store` and ship in
the image, but it is never a tool, compilation input, or execution input to a
source-built output. Only the explicit read-only `payload_inputs` channel may
carry it into image composition or runtime association; td runs it only
through the application confinement path described below and in
`APPLICATIONS.md`.

## Control plane

The host supplies a pinned Rust toolchain to compile host-seeded instances of
td's control-plane programs (`td-builder`, `td-recipe-eval`, and the `td-net`
multicall). These may evaluate recipes, fetch declared fixed-output sources,
construct sandboxes, and place outputs. The staged host-built `td-builder` has
explicit `ControlPlaneBuilder` provenance and may run only as the derivation
implementation, never through a recipe's `PATH` or argv. It is not a recipe
tool, target artifact, or runtime input, and no other host-built program may be
exposed to recipe steps.

Program role and build provenance are separate. With its target Rust
toolchain, td rebuilds those same control-plane programs as target recipes.
Those target-built copies are distribution artifacts and ship so a td system
can build itself. Only instances built by the host Rust seed are excluded from
the distribution closure.

## Target artifact graph

The target graph builds directly into `/td/store` from pinned upstream
sources and imported runtimes. Its trust roots are the pinned Debian build
root (including its reviewed runtime exports) and the upstream Rust
toolchain (both below):

```text
pinned sources, in the Debian build root
  -> imported Debian glibc and libgcc, the kernel, C libraries and td-link
  -> upstream Rust toolchain, retargeted onto td glibc
  -> target-built td control plane, tools, and Rust userland -> image
```

No compiler in this graph is source-bootstrapped: the build root's GCC and
binutils and the toolchain's rustc and LLVM compile the source-built outputs;
glibc and libgcc are imported prebuilt. The closure query's provenance claim (`APPLICATIONS.md` §B.8) names them as
trust roots wherever they reach.
What td establishes is that nothing undeclared enters a build. Recipe steps
may execute only unmarked outputs of earlier recipes, executables created by
the current build, and, in a recipe that declares it, the Debian build root's
tools. Host `/bin`, `/usr`, ambient `PATH`, and arbitrary host store paths are
never target inputs.

`td-builder build` stages declared inputs and sets compatibility `NIX_STORE`
to the active td store. A `TD_STORE_DIR=/td/store` build is hashed for and
built at `/td/store`; there is no post-hoc prefix rewrite and no Nix runtime
dependency.

## Foreign application payloads

td's target graph contains no foreign binary other than the Debian build
root's tools and reviewed runtime exports, the upstream Rust toolchain,
the non-executable pinned upstream data below and the device firmware
after it. A third-party application is a marked foreign prebuilt
payload in `/td/store`, not a build-tool trust root. The mark is load-bearing:

- ordinary input channels refuse it; only `payload_inputs` may carry it as
  read-only data for image composition or runtime association;
- no recipe may execute it, link against it, or name it as a tool;
- closure queries exclude and report it in provenance claims;
- its source pin carries a reviewed URL, compiled expected SHA-256, and the
  mark that propagates to derived outputs.

Adding an application is a reviewed pin. Adding a class of foreign output is
an amendment to this file.

Flathub applications and runtimes are the second class (`APPLICATIONS.md`
§W.9). Their bytes are neither pinned nor in
`/td/store` or the image: what is reviewed is the application's declaration
(repository, exact app and runtime refs and td's own policy) and Flathub's
signing key compiled into td-net. The payload is fetched on demand into the
application account's private cache and admitted only under that signature,
exact ref bindings, a monotonic commit time and §B.3.1's bounds and
validators. It is never an input to any recipe, is outside every closure
and source-bootstrap claim, and runs only through the same confinement.

The image asserts that a dynamic payload's `PT_INTERP` is absent from the
image root, but this does not prove the bytes can execute only in a jail: an
explicit store `ld.so` can load them, and static payloads have no interpreter.
The claim is narrower and exact: no foreign application binary is a tool,
compilation input, or execution input to a source-built output. td's supported
run path puts applications behind td-owned namespace, seccomp, D-Bus, portal,
and Wayland boundaries. See `APPLICATIONS.md` §B.8.

Image-shipped applications ride the system deployment, so independent
side-by-side retention is deferred for them (`APPLICATIONS.md` §W.2); a
Flathub application keeps its current and previous generations in its
account's cache (§W.9).

## Pinned upstream data

Some data td takes as upstream built rather than rebuilding it from
source: curl's rendering of Mozilla's trust store, and the Nerd Fonts
JetBrains Mono release, whose compiled TrueType faces td-ui parses. Such
a pin carries a reviewed URL and SHA-256 like any source, and stays
unmarked because nothing executes, links, or loads it as code: td's
builder copies it into zone-one outputs, and td-built programs parse it
under their own bounds. td-ui's font reader runs no hinting bytecode. It
is data inside the target graph, not source-built; the closure query
counts it among the audited seeds rather than naming it. Adding a pin of
this class is a reviewed pin; data a td program would execute or load as
code belongs to no class here and needs an amendment to this file.

## Device firmware

Some hardware runs code td cannot build: the ThinkPad T14s Gen 4 AMD's
Qualcomm WCN6855 Wi-Fi loads its firmware from the host when its driver
probes. td takes such firmware as upstream bytes from linux-firmware's
tagged tree, each file a pin with a reviewed URL and SHA-256, listed in
`DEVICE_FIRMWARE` (`recipes/src/source_pins.rs`). Only the peripheral
it is written for runs it. The kernel recipe copies the pinned files
unmodified into its build tree and embeds them with
`CONFIG_EXTRA_FIRMWARE`; the kernel's firmware loader hands them to the
device's driver; no recipe step executes, links or loads them as host
code, and the image carries them only inside the kernel, with their
notices as data beside it. Only the kernel recipe names a roster pin,
which a catalog test holds. They stay
unmarked, like the data above, because nothing in zone one runs them,
and the closure query counts them among the audited seeds. Being code,
they are also named: one `device-firmware` line per pin, beside the
trust roots, for every closure holding the kernel that embeds them.
What td does not establish is anything about that code: a device with
firmware can reach host memory by DMA, which an IOMMU, where the kernel
drives one, bounds (with lazy unmapping), and the WCN6855 answers QMI
over the QRTR socket family, which td-jail's filter keeps from
applications (not from unjailed session processes). Adding firmware
for a device is a reviewed pin in that roster; code the host CPU
executes, CPU microcode included, belongs to no class here and needs
its own amendment.

## Debian build root

td's C toolchain is the Debian build root. It is a build environment, not
an input: a pinned Debian image layer plus pinned .debs from
snapshot.debian.org, resolved into
`recipes/src/debian-root.lock` by `td-feed debian-root resolve`, and
assembled by td-builder into one input-addressed store path whose NAR hash
the lock pins. A recipe that declares `build_root` runs with that tree as
its read-only sandbox `/`, so its steps may run Debian's gcc, binutils, make
and the rest, with writable scratch where the minimal root had it
(`UNSAFE.md` lists the mounts). What td enforces:

- its pins are a class of their own: no input channel, seed or source-pin
  lookup resolves one, and the sandbox binds as `/` only the path the
  compiled lock names, at its pinned hash, vouched by a db typed
  `BuildRoot`, never a builder or tool input;
- an output built in it may not reference its store path, and no file in
  that output may be an ELF naming an interpreter or RUNPATH outside the
  store (a non-executable debug-only companion, whose loader ranges hold
  no data, is judged by the runtime it pairs with), or a symlink to an
  absolute path outside the store.

The `glibc-x86-64` and `gcc-runtime` recipes import a reviewed subset of
that root into normal store outputs: Debian's glibc libraries, headers,
startup objects and timezone tools, and its libgcc libraries and GCC startup
objects. They compile nothing. `debianRuntime` names a versioned transform;
its compiled file roster selects the bytes, normalizes the multiarch layout,
rewrites libc/libm linker scripts and retargets ELF interpreters to the store.
Changing the transform or roster requires advancing its version token.
No package maintainer script runs. These are unmarked upstream runtime trust
roots, not application payloads. The closure query names each with
`upstream-runtime\tdebian\t<recipe>` alongside its build-root provenance.

Other recipes link only against these declared store libraries and their
own declared inputs, never ambient build-root libraries. Static objects
cannot be distinguished after linking, so their link-map checks establish
that boundary. The imported libraries and their code inside static outputs
are profiler coverage boundaries, not source-built or frame-pointer claims.
The source GCC/glibc bootstrap and its source pins are absent.

Changing the root's packages is a reviewed lock change.

## Rust toolchain and dependencies

td uses upstream's prebuilt Rust 1.96.0 for `x86_64-unknown-linux-gnu`: the
rustc, rust-std, Cargo and Clippy release components, each a source pin with
an upstream URL and its channel-manifest SHA-256. The `rust-toolchain`
recipe's `rust-dist` transform assembles them, links td glibc,
gcc-runtime's libgcc_s and td zlib into the tree, and relinks every shipped
executable onto td glibc's loader; it compiles nothing. The toolchain,
rust-lld and its LLVM included, is a build-tool trust root like the Debian
build root: unmarked, executed by recipes, and named
by a provenance claim (`upstream-rust`) for every closure that contains
it. The image ships launchers, not the toolchain: `td-rust` downloads the
same pinned components on first use into the invoking account's private
cache. Recipe builds retain their declared `rust-toolchain` input; the
account cache is never a target recipe input. `td-link/DESIGN.md` specifies
the runtime linker and `APPLICATIONS.md` §G specifies the downloader.
Every Rust output links its prebuilt standard library. A
stronger claim needs a separately specified diverse bootstrap or
diverse-double-compilation proof. Every Rust output a td recipe builds,
Cargo's build scripts and proc macros included, links through `td-link`
(`td-link/DESIGN.md`): a dependency-free driver that takes rustc's
gcc-style link line, refuses compile requests, unknown linker options,
host library paths on its line and inputs that are not ELF objects or
ordinary archives, and runs the toolchain's rust-lld with td glibc's and
gcc-runtime's start files and libraries, so no GCC or binutils takes part
in the link. td-link is itself a Debian build-root member: the root's GCC
links its throwaway first stage, and its claim to link without GCC rests
on its second and third stages linking byte-identically. The image ships
td-link, not GCC or binutils: its `/bin/cc` is td-link, so rustc's and
Cargo's default linker, and the native control-plane build on a td host,
link through it, and a td host compiles no C outside the build root.
Every Cargo recipe builds in the Debian build root: the C its build
scripts compile is the root's GCC, which the runner's CFLAGS forbid to
link, against td glibc's headers and GCC's own freestanding headers,
archived by the root's `ar`, and the root's `objcopy` splits its
companions. Each object the build leaves must show it: its dependency
list resolves into those headers or the build's trees, and the recipe
declares how many objects there are. Every other recipe that runs
rustc directly builds there too, but td-link-test, which builds outside
it to prove td-link needs no GCC or binutils, and the root's `objcopy`
splits its companions.

Cargo dependency sources are fixed-output inputs selected by committed
`Cargo.lock` checksums. Builds run offline. Git dependencies are unsupported
without explicit sign-off and a fixed-output representation pinned by commit
and archive hash. Build scripts, proc macros, C, and assembly may use only
td-built tools, the pinned Rust toolchain and, for C and assembly, the
Debian build root's compiler and archiver. New td-owned shipped userland
should be Rust built with that toolchain.

The final image uses uutils for its core userland and carries no GNU
userland. glibc and libgcc, the kernel, boot/firmware components, and
explicitly reviewed non-Rust packages are the exceptions; it ships no C
compiler.

## Target user-mode profiling contract

Every source-built user-mode artifact that ships and can appear in a runtime
stack keeps frame pointers. This includes the source-built
libraries and userland closure; td-owned programs, dependency crates, and
native objects compiled by Cargo build scripts. The upstream Rust toolchain is
not source-built: its own objects carry no debug companions, and the prebuilt
standard library every Rust output links keeps frame pointers in only part of
its code; `td-profiler/DESIGN.md` names both as coverage boundaries. Debian
glibc and libgcc likewise retain upstream coverage: their shared objects
have no td companions and their static code is named in every affected
output's coverage marker. It is one target-wide policy, not a per-recipe
release-profile choice. A recipe may not weaken it without amending this contract and `td-profiler/DESIGN.md`.

Every such ELF also has a deterministic debug companion in the same recipe
output and system image. A companion carries the ordinary symbol table and no
DWARF: profiler output attributes functions, and source lines are
deliberately unavailable. The Debian build root, build-only
intermediates, the kernel, firmware, marked foreign payloads, the upstream
Rust toolchain, hand-written assembly, and the exact coverage and
reproducibility rules are specified in `td-profiler/DESIGN.md`; profiler
output reports every boundary explicitly. The image's object index leaves the
toolchain out, so its frames report as unresolved symbol coverage.

The first `td-profiler/DESIGN.md` implementation increment enforces the
producer side in compiler flags, per-output runtime/debug pair checks, size
ceilings, and reproducibility oracles. The image-integration increment owns
the whole-deployment coverage walk; until that increment is present, this
paragraph is a target contract rather than a whole-image completeness claim.

# Principles

1. **No undeclared dependencies.** Builds are offline except for declared
   fixed-output fetches. Never make a build pass by reaching outside its
   sandbox or adding an undeclared input.
2. **Avoid external dependencies.** An external dependency is code from
   outside this repository. Get explicit sign-off before adding one and call
   it out in the landing commit. The dependency-free Rust surfaces should
   remain pure `std` plus td's own crates: a sibling `td-*` crate in this
   repository, depended on by path as td-ui is, is not external and needs no
   sign-off. Code two crates need lives in one such crate, not in a copy per
   consumer.
3. **Avoid shell.** Prefer dependency-free Rust. Existing recipe shell is not
   precedent for new control-plane or target logic.
4. **Migrations are atomic.** Cut over completely and delete the old mechanism
   in the same landing. Git history carries dates and change narratives.
5. **No maintainer-run server infrastructure.** No package repository, binary
   cache population service, update server, or mirror. Recipes travel in git;
   pins use upstream-owned URLs plus SHA-256; everything else builds locally.
   `td-feed` is a host cache, and `td-subst` is a consumer for a cache nobody
   is required to operate. Never pin a td-produced artifact that would require
   td to publish or host it.
6. **Updates are a git pull, not a package download.** The recipe graph is the
   distribution. System deployments retain `current` and `previous` and boot
   one; the future application tier retains versions side by side and selects
   one with a pointer. The system runs one thing; applications run many.
7. **Passwordless, not authorization-less.** Human authentication defaults to
   TPM plus PIN, with FIDO2 plus PIN as an alternative. PINs authorize
   hardware-protected keys; TPM possession alone is device binding, not user
   identity. `td-install/ENCRYPTION.md` owns disk unlock and recovery policy.
   Elevation is one named operation with typed, descriptor-pinned arguments
   and one consent bound to that request, never a shell, account password,
   remembered approval, or grace window. Sensitive protector changes require
   fresh hardware-backed authentication. An unlocked personal notebook
   (`td-secret/PORTABLE.md`) is not elevation: its unlock authorizes its own
   entry writes until it locks, and no protector change or import. The
   compositor must provide a secure attention and trusted-input path before
   this ships. Other secrets require a second recovery token or an explicit
   unrecoverability decision.

Trusted live-media installation is the explicit exception to the secure
attention requirement: its isolated installer wizard confirms the exact
reviewed disk and erase/preserve mode directly. The live wizard and its
ordinary input are trusted for installation; this is not installed-session
elevation or authentication. See `td-install/INSTALLER.md`.

Principle 7 is a target, not a current claim. The stock VM writes an
empty shadow field for `tester` and auto-logs in. Root is locked (`!`)
and there is no `su`; no console or SSH login reaches root.
Administration is the session owner's approval-key operations; without
consent, recovery is td-boot's automatic rollback or physical access
(`APPLICATIONS.md` §L.1, "Recovery").
The paired compositor and `td-authd` provide physical secure attention,
explicit token enrollment and token-gated secret release. File and
TPM-only stores cannot serve application credentials. Typed credential
writes require their own presented token operation; they cannot use an
existing session release. The stock VM remains unenrolled. See
`APPLICATIONS.md` §L.1 and `td-login/THREAT-MODEL.md`. The
device-bound encrypted default, which the installer chooses on machines
with a usable TPM and a keyboard console, releases storage to TPM
possession alone; it is device binding under this principle and advances
no authentication or protector-authorization claim.
The TPM-free login-key tier (`td-login/TOKEN-LOGIN.md`) lets the
installed account's owner enroll FIDO2 keys on the attention screen,
trading auto-login and the ordinary SSH form for a key plus PIN at boot
and session lock. Until a key is enrolled, login, SSH, updates and
the lock change only where the login directory is invalid, though every
paired session consumes `Super+l` and shows `L`. That document says
when the tier may be called usable and what it does not protect.

# Tests

The single full pass/fail command is:

```text
cargo run --release --manifest-path builder/Cargo.toml -- check
```

The system-level qemu oracles are the separate `td-builder check
integration` tier, which main runs after a landing and no branch's
`ready` runs; `DEVELOPMENT.md` says how.

Build the builder with:

```text
cargo build --release --manifest-path builder/Cargo.toml
```

Host test and review preparation owns standard compiler commands, headers
and libraries. A missing `cc` is an environment defect to fix there; agents
should not repair it with per-test compiler overrides or target recipe inputs.
See `DEVELOPMENT.md` and `td-agent/DESIGN.md` for host preparation.

Recipes should test their realized output. For bounded preflight selection:

```text
td-builder affected-checks
td-builder affected-checks --run
```

It compares against `origin/main` (falling back to `main`) and includes dirty,
staged, and untracked files by default. `--committed-only` measures only the
branch diff; `--path FILE` explains one path's mapping.

# Parallel work

`DEVELOPMENT.md` carries the complete mutation workflow. The always-loaded
boundaries are:

- change work happens in an isolated worktree branch, at the location
  specified in `DEVELOPMENT.md`;
- agent review copies, continuations, and other disposable scratch belong
  under that worktree's `.td-build-cache/agent-scratch/`, never as loose
  `/tmp` trees. Each agent owns its scratch and deletes it before finishing,
  restoring owner directory permissions when fixtures removed them;
- new or changed test fixtures own their temporary trees with the shared
  `td-fs::TempDir` guard
  (the control-plane workspace includes its source only under `cfg(test)`),
  so normal return and assertion unwinding both reclaim them;
- reserve the `-rolling` suffix for a long-lived stacked workstream that
  continues after partial landings; use a descriptive non-rolling branch for a
  one-off change;
- one self-contained, green commit is one independently landable increment;
- never use `git stash` in this repository;
- never signal a process by matching its command line: every worktree runs the
  same binary path, so `pkill -f td-builder` cannot mean "mine". Stop your own
  check run with `td-builder stop`, or signal a pid you recorded yourself;
- `td-builder ready` is the pre-push gate. `--record-only` is not a substitute;
- a change request authorizes pushing the ready branch to `origin`; that branch
  is the PR the separate integrator lands.

# Code review

- Each commit receives two independent reviews at high effort, using
  `opus-latest` and `gpt-sol-latest` or the approved subscription
  alternatives in `DEVELOPMENT.md`. Invoke a fresh harness subagent
  with the required model when supported; otherwise use the subscription
  alternatives or `td-agent review` defined in `DEVELOPMENT.md`. That
  document defines the standing approval and durable record. One panel cycle
  is scheduled per commit; later passes use the review subagent alone,
  and re-running the panel needs a named escalation trigger. Stop and ask
  when the passes stop converging.
- The acting agent reconciles every finding and records its own summary,
  concrete reviewer identities, and checks in the commit message.
- Retained td-agent review traces are user-owned audit records, outside
  disposable sessions. Preserve them during cleanup; delete them only at
  the human's explicit request. `DEVELOPMENT.md` distinguishes their paths
  from disposable reports and review workspaces.

A documentation-only commit may use `Review-waiver: docs-only`; `ready`
verifies that every touched path ends in `.md`. Other unavailable-reviewer
waivers require a reason and the human approver named in the trailer.

Commit messages are the durable review record. Put rationale, design
decisions, findings and dispositions, and verified-red evidence there. Hard
wrap bodies at 72 columns. The review/check trailer block must close the
message.

# Rust code

- Every Rust file is in rustfmt's default style, including files compiled
  only through `include!` or `#[path]`; the `cargo-test` and `net-test`
  preflights enforce it. `DEVELOPMENT.md` says how to format.
- New or changed production code must not add `unwrap()`, `expect()`,
  `panic!`, `unreachable!`, `todo!`, `unimplemented!`, or panicking indexing.
  Return `Result`/`Option`, propagate with `?`, and use `.get()` for untrusted
  indices. Inline `#[cfg(test)]` code may opt out locally. Existing
  grandfathered production allowances are migration debt: do not widen them
  or use them as precedent.
- Code must build without rustc's `unused` warnings (dead code, unused
  imports, variables, `#[must_use]` results). The clippy legs deny them
  (`-D unused`; `-D warnings` for the crypto crates) in shipped targets, and
  in test targets of crates declaring `clippy-all-targets`. Keep a
  test-only helper inside its `#[cfg(test)]` module.
- `unsafe` is confined to the syscall surfaces and the separately recorded
  test-only allocation instrumentation in `UNSAFE.md`. A new
  surface, syscall, value-pinned request, or scoped allow requires an amendment
  there and in any component design/threat model in the same landing. Each
  crate's confinement tests must pin the source-level contract the compiler
  cannot express.
- `builder`, `recipes`, and `engine` are one zero-external-dependency workspace.
  Target crates and the host tool `td-vm` have standalone
  locks that list their own package and, at most, other `td-*` roster crates
  they depend on by path (except the named crypto admission below): no registry
  or git crate, and no path outside the roster, so engine sources reach a target crate only as shared source. A
  sibling is declared only as `NAME = { path = "../NAME" }` on its own line
  under a bare `[dependencies]` or `[dev-dependencies]` header (`cargo add`
  also writes a `version`; remove it). The manifest is where the guard
  establishes what a path points at, since a lock cannot say; the lock is then
  held to exactly the declared closure. So that a line reader can be exact, a
  roster manifest keeps its table headers bare and its values on one line and
  carries no `[target]`, `[patch]` or `[replace]` table; a roster crate
  carries no `.cargo` directory of its own, the repository's `.cargo` config
  carries no `paths`, `[patch]`, `[source]` or `include` key, and a declared
  `test-args` or `gate-test-args` carries no `--config` or `-Z`.
  `td-net` retains its existing reviewed vendored closure. The other named
  exception is `td-crypto`'s private Rustls/AWS-LC closure: exact manifests,
  locks, root Cargo configuration and active features are pinned in `builder/src/crypto_policy.rs`.
  `td-mta` depends directly on local `td-crypto` and std-only `td-civil`, `td-header`
  plus `td-html`, `td-json`, `td-mime` and `td-nfc`, and the approved
  private `tokio-postgres` 0.7.18 closure with its runtime feature disabled.
  PostgreSQL is the sole mail backend described in `td-mta/POSTGRES.md`.
  Its exact manifest, lock and active features are pinned in the same policy.
  Its lock includes this closure and the std-only `td-fs` test helper.
  All nine
  local manifests and locks are pinned. No other roster crate may depend
  on `td-crypto` or `td-mta`, directly or transitively; `td-header`,
  `td-civil`, `td-html`, `td-json`, `td-mime` and `td-nfc` remain
  ordinary std-only shared crates.
  `td-crypto/DESIGN.md` owns offline preparation and crypto confinement;
  `td-mta/STORAGE.md` owns local scratch and the storage boundary. No other
  roster crate may consume the PostgreSQL closure, directly or transitively.
  Any new external dependency needs principle-2 sign-off.
- A new standalone crate joins the gate by EXISTING: `builder/src/affected.rs`
  discovers every `td-*/Cargo.toml` at the repo root and derives both the
  dependency-free lock roster and the cargo test/clippy commands from it. Commit
  the crate's `Cargo.lock` or the gate reds. A crate wanting non-default gating
  declares it in its own `[package.metadata.td-gate]`; a crate that cannot wear
  the `td-` prefix is an amendment to that discovery rule.
  The in-sandbox gate `builder/src/gate_defs/325-cargo-test.rs` asks for the
  same roster (`td-builder gate-crates`), so one derivation feeds both tiers.
  A crate whose in-sandbox suite differs from the host preflight's declares
  `gate-test-args` beside `test-args`.
- Use `std`, not `no_std`. Allocate buffers and collections outside hot loops
  where practical. Keep comments terse and explain a non-obvious why; design
  rationale and review history belong in the commit message or normative doc.
- Declare a constant list as a slice, `const NAMES: &[&str] = &[...]`: two
  branches that each add an entry and bump a `[&str; N]` can merge without
  conflict into a mis-sized array that does not compile. Keep `[T; N]` only
  where the length is itself the invariant or entries are read by position.
  The builder/recipes/engine workspace follows this; convert a `td-*` crate's
  list when touching it.

Keep this root file focused on rules every task needs. Put component detail in
the routed normative document and tool-specific procedure in `DEVELOPMENT.md`.
