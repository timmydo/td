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
- For application packaging or confinement, read `APPLICATIONS.md`.
- Before changing or adding `unsafe`, read `UNSAFE.md` and the touched crate's
  normative document.
- For login or credentials, read `td-login/THREAT-MODEL.md`.
- For compositor/UI, service supervision, or installation, read the matching
  `td-compositor/DESIGN.md`, `td-svc/DESIGN.md`, or `td-install/DESIGN.md`.
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

td has two bootstrap graphs. Do not confuse the host control plane with the
target distribution artifact graph.

## Trust zones

td has three trust zones. Zone one is the source-bootstrapped target artifact
graph. Zone two is the host control plane that evaluates and builds that graph
but never enters it. Zone three is each marked foreign prebuilt application
payload. A zone-three payload may live in `/td/store` and ship in the image,
but it is never a tool, compilation input, or execution input to a
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

Program role and build provenance are separate. After td has a target Rust
toolchain, it rebuilds those same control-plane programs as target recipes.
Those target-built copies are distribution artifacts and ship so a td system
can build itself. Only instances built by the host Rust seed are excluded from
the distribution closure.

## Target artifact graph

The target graph begins with the tiny, auditable stage0-posix seed and builds
directly into `/td/store`:

```text
stage0-posix -> Mes/MesCC -> TinyCC -> early GNU tools
  -> iterative binutils/GCC/glibc ladder -> native GNU build platform
  -> retargeted Rust bootstrap snapshot -> source-built stage1
  -> full-bootstrap source-built stage2 Rust and Cargo
  -> target-built td control plane, tools, and Rust userland -> image
```

The stage0/Mes ladder is the provenance boundary: every later target
executable descends from declared seeds and source, which keeps undeclared host
binaries out and makes the from-scratch chain reproducible and checkable. The
compiler/libc path is iterative. Recipe steps may execute only audited seed
executables, unmarked outputs of earlier recipes, and executables created by
the current build. Host `/bin`, `/usr`, ambient `PATH`, and arbitrary host store
paths are never target inputs.

`td-builder build` stages declared inputs and sets compatibility `NIX_STORE`
to the active td store. A `TD_STORE_DIR=/td/store` build is hashed for and
built at `/td/store`; there is no post-hoc prefix rewrite and no Nix runtime
dependency.

## Foreign application payloads

td's bootstrap graph contains no foreign binary other than its declared
bootstrap seeds. A third-party application is a marked foreign prebuilt
payload in `/td/store`, not a bootstrap seed. The mark is load-bearing:

- ordinary input channels refuse it; only `payload_inputs` may carry it as
  read-only data for image composition or runtime association;
- no recipe may execute it, link against it, or name it as a tool;
- closure queries exclude and report it in source-bootstrap claims;
- its source pin carries a reviewed URL, compiled expected SHA-256, and the
  mark that propagates to derived outputs.

Adding an application is a reviewed pin. Adding a class of foreign output is
an amendment to this file.

The image asserts that a dynamic payload's `PT_INTERP` is absent from the
image root, but this does not prove the bytes can execute only in a jail: an
explicit store `ld.so` can load them, and static payloads have no interpreter.
The claim is narrower and exact: no foreign application binary is a tool,
compilation input, or execution input to a source-built output. td's supported
run path puts applications behind td-owned namespace, seccomp, D-Bus, portal,
and Wayland boundaries. See `APPLICATIONS.md` §B.8.

Applications currently ship inside the system image, so independent
side-by-side application retention is deferred (`APPLICATIONS.md` §W.2).

## Rust bridge and dependencies

Rust enters only after the native GCC/glibc platform and GNU build userland
exist. td pins the Rust source and exact upstream bootstrap snapshot. The
snapshot is transformed to run on td's declared runtime and is used only as
stage0. Full-bootstrap builds stage1, rebuilds rustc and the in-tree standard
library as stage2, and builds in-tree Cargo. No downloaded Cargo, library,
stage0 byte, or prebuilt LLVM enters a final distribution closure.

The snapshot remains a bootstrap trust root; source rebuilding alone does not
defeat trusting trust. A stronger claim needs a separately specified diverse
bootstrap or diverse-double-compilation proof.

Cargo dependency sources are fixed-output inputs selected by committed
`Cargo.lock` checksums. Builds run offline. Git dependencies are unsupported
without explicit sign-off and a fixed-output representation pinned by commit
and archive hash. Build scripts, proc macros, C, and assembly may use only
td-built tools. After the bridge, new td-owned shipped userland should be Rust
built with the source-built stage2 toolchain.

The final image uses uutils for its core userland and does not carry the GNU
bootstrap userland forward. The source-bootstrap toolchain, glibc, kernel,
boot/firmware components, and explicitly reviewed non-Rust packages are the
exceptions.

## Target user-mode profiling contract

Every source-built user-mode artifact that ships and can appear in a runtime
stack keeps frame pointers. This includes the source-built compiler, libc and
userland closure; rustc, the in-tree standard library, Cargo, td-owned
programs, dependency crates, and native objects compiled by Cargo build
scripts. It is one target-wide policy, not a per-recipe release-profile
choice. A recipe may not weaken it without amending this contract and
`td-profiler/DESIGN.md`.

Every such ELF also has a deterministic, path-remapped debug companion in the
same recipe output and system image. Bootstrap seeds, build-only intermediates,
the kernel, firmware, marked foreign payloads, hand-written assembly, and the
exact coverage and reproducibility rules are specified in
`td-profiler/DESIGN.md`; profiler output reports every boundary explicitly.
The Codex 0.148.0 dynamically linked CLI is the one named source-line
boundary: its structurally checked line program exceeds the profiler's
bounded per-object reader, so its companion retains the line program and
ordinary symbols while td-profiler reports source-line attribution
unavailable. The exact producer ceiling, marker, and retained-section policy
are specified in that design.

The first `td-profiler/DESIGN.md` implementation increment enforces the
producer side in compiler flags, per-output runtime/debug pair checks, size
ceilings, and reproducibility oracles. The image-integration increment owns
the whole-deployment coverage walk; until that increment is present, this
paragraph is a target contract rather than a whole-image completeness claim.

# Principles

1. **No undeclared dependencies.** Builds are offline except for declared
   fixed-output fetches. Never make a build pass by reaching outside its
   sandbox or adding an undeclared input.
2. **Avoid external dependencies.** Get explicit sign-off before adding one
   and call it out in the landing commit. The dependency-free Rust surfaces
   should remain pure `std`.
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
7. **Passwordless, not authorization-less.** Human authentication uses a FIDO2
   token; secrets at rest are hardware-sealed; TPM possession is device
   binding, not user identity. Elevation is one named operation with typed,
   descriptor-pinned arguments and one consent bound to that request, never a
   shell, password, remembered approval, or grace window. The compositor must
   provide a secure attention and trusted-input path before this ships.
   Recovery is a second token enrolled when the secret is created, or the
   secret is explicitly unrecoverable.

Principle 7 is a target, not a current claim. The stock VM writes empty shadow
fields for `root` and `tester`, auto-logs in, and retains `su` as an
administrative escape hatch. FIDO2, TPM sealing, secure attention, and
`td-authd` are not built. Do not make a user-facing flow depend on the current
escape hatch; see `APPLICATIONS.md` §L.1 and `td-login/THREAT-MODEL.md`.

# Tests

The single full pass/fail command is:

```text
cargo run --release --manifest-path builder/Cargo.toml -- check
```

Build the builder with:

```text
cargo build --release --manifest-path builder/Cargo.toml
```

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

- change work happens in an isolated worktree branch;
- reserve the `-rolling` suffix for a long-lived stacked workstream that
  continues after partial landings; use a descriptive non-rolling branch for a
  one-off change;
- one self-contained, green commit is one independently landable increment;
- never use `git stash` in this repository;
- `td-builder ready` is the pre-push gate. `--record-only` is not a substitute;
- a change request authorizes pushing the ready branch to `origin`; that branch
  is the PR the separate integrator lands.

# Code review

- Each commit receives the required independent subagent and cross-model
  reviews over its own diff. One panel cycle is scheduled per commit; later
  passes are the review subagent alone, and re-running the panel needs a
  named escalation trigger. Stop and ask when the passes stop converging.
- The acting agent reconciles every finding and records its own summary,
  reviewer identities, and checks in the commit message.

A documentation-only commit may use `Review-waiver: docs-only`; `ready`
verifies that every touched path ends in `.md`. Other unavailable-reviewer
waivers require a reason and the human approver named in the trailer.

Commit messages are the durable review record. Put rationale, design
decisions, findings and dispositions, and verified-red evidence there. Hard
wrap bodies at 72 columns. The review/check trailer block must close the
message.

# Rust code

- New or changed production code must not add `unwrap()`, `expect()`,
  `panic!`, `unreachable!`, `todo!`, `unimplemented!`, or panicking indexing.
  Return `Result`/`Option`, propagate with `?`, and use `.get()` for untrusted
  indices. Inline `#[cfg(test)]` code may opt out locally. Existing
  grandfathered production allowances are migration debt: do not widen them
  or use them as precedent.
- `unsafe` is confined to the syscall surfaces recorded in `UNSAFE.md`. A new
  surface, syscall, value-pinned request, or scoped allow requires an amendment
  there and in any component design/threat model in the same landing. Each
  crate's confinement tests must pin the source-level contract the compiler
  cannot express.
- `builder`, `recipes`, and `engine` are one zero-external-dependency workspace.
  Target crates and `td-review` are standalone one-package locks. The `td-net`
  multicall is the sole external-dependency tier and may use only its existing
  reviewed vendored closure. Any new dependency needs principle-2 sign-off.
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

Keep this root file focused on rules every task needs. Put component detail in
the routed normative document and tool-specific procedure in `DEVELOPMENT.md`.
