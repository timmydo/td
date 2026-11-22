# AGENTS.md — td

You are one of possibly several agents building a functional Linux
distribution. This file is the always-loaded contract. It holds the rules
every task needs and routes to the rest; state a rule in one place. Status,
rosters, version numbers and exceptions to exceptions belong in the owning
document, not here. `td-builder ready` refuses a branch that grows this
file past its word budget (`AGENTS_WORD_BUDGET`, `builder/src/ready.rs`).

# What to read

Read this file completely. Load other documents only when the task needs
them. They are normative: code that changes an invariant one states amends
it in the same landing.

- Before changing files, read `DEVELOPMENT.md` completely: the worktree,
  commit, review, ready and submission workflow, and the crate manifest and
  lock rules.
- Before changing recipe inputs, source pins, the Debian build root, the
  Rust toolchain, what an image ships, or how a foreign payload enters or
  runs, read `TRUST.md`.
- A crate `td-X` with a `td-X/DESIGN.md` is governed by it; read it before
  changing the crate or code that relies on its contract.
- Documents not named for a crate:
  - outdated pins (sources, the Rust toolchain, the Debian build root,
    Cargo locks): `UPDATING.md`;
  - application packaging or confinement: `APPLICATIONS.md`;
  - the account a workload (an application, a td-sandbox sandbox or a
    td-agent workspace) runs as, shared runtimes, or grants of the human's
    files to a workload: `WORKLOADS.md`;
  - changing or adding `unsafe`: `UNSAFE.md` and the touched crate's
    normative document;
  - login or credentials: `td-login/THREAT-MODEL.md`;
  - trusted-main remote administration: `td-update/ADMIN.md`;
  - disk encryption or session unlock: `td-install/ENCRYPTION.md`;
  - target compiler flags, ELF debug handling, profiler code or profiler
    image integration: `td-profiler/DESIGN.md`.
- Topics whose crate's name does not say so: the FIDO2 client, and the
  SHA-256, HMAC and HKDF the boot and login crates share (`td-fido`);
  the rustls provider over td's primitives (`td-tls-provider`); the Rust
  link driver (`td-link`); the transport to a td program on another
  machine over ssh (`td-remote`); `mkfs.btrfs` (`td-btrfs`); extra data
  volumes and removable media (`td-disks`); LUKS2 (`td-luks`); mail
  serving, storage and submission (`td-mta`); service supervision
  (`td-svc`); allocation helpers and guards (`td-alloc`); the shared UI
  toolkit (`td-ui`); the agent harness (`td-agent`); the application
  manager (`td-software`); td's web browser (`td-web`) and its page
  engine (`td-page`); document layout (`td-layout`); Unicode
  normalization (`td-nfc`); header lexical syntax (`td-header`).

# Request scope

- For requests to answer, explain, review, audit, diagnose, or plan: inspect
  the relevant material and report the result. Do not edit, commit, push, or
  otherwise change state unless the request also asks for a change.
- For requests to change, build, or fix: complete the requested in-scope
  change, validate it, carry it through the review and ready workflow, and push
  its branch to `origin`. The pushed branch is the submission. Stop at a local
  branch only when the user explicitly asks for local or draft work.

# Build and trust model

`TRUST.md` is normative for the build graphs and every trust class. Its
sections (Trust zones, Control plane, Target artifact graph, Foreign
application payloads, Pinned upstream data, Device firmware, CPU microcode,
Debian build root, Rust toolchain and dependencies, Target user-mode
profiling contract) once lived here; a citation of `AGENTS.md` by one of
those names means the `TRUST.md` section.

## Trust zones

td has two build graphs; do not confuse them. Zone one is the target
artifact graph, built into `/td/store` from pinned sources. Zone two is the
host control plane (`td-builder`, `td-recipe-eval`, `td-net` built by the
host Rust seed), which evaluates and builds that graph but never enters it.
Zone three is each marked foreign prebuilt application payload. Every task
keeps these invariants:

- Nothing undeclared enters a build. Recipe steps execute only unmarked
  outputs of earlier recipes, executables the current build created, and,
  in a recipe that declares it, the Debian build root's tools. Host `/bin`,
  `/usr`, ambient `PATH` and arbitrary host store paths are never target
  inputs.
- The trust roots are the pinned Debian build root, with the glibc and
  libgcc imported from it, and the upstream Rust toolchain; no compiler is
  source-bootstrapped. Every Rust output links through `td-link`.
- A foreign payload's mark is load-bearing: only `payload_inputs` carries
  it, no recipe executes, links or names it as a tool, and td runs it only
  through application confinement (`APPLICATIONS.md`).
- Every shipped source-built user-mode ELF keeps frame pointers and has a
  debug companion (`td-profiler/DESIGN.md`).
- Adding a pin within a class is a reviewed pin. Adding a class of foreign
  output, upstream data, or code td does not build amends `TRUST.md`.
- Do not build, run or check td's control plane in a workspace that names a
  Flathub runtime.

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
   consumer. Cryptography is not exempt: td may write its own primitives, and
   a crypto library is an external dependency like any other. New ones live
   in `td-primitives`, small enough to audit, under the rules its design
   states.
3. **Avoid shell.** Prefer dependency-free Rust. Existing recipe shell is not
   precedent for new control-plane or target logic.
4. **Migrations are atomic.** Cut over completely and delete the old mechanism
   in the same landing. Git history carries dates and change narratives.
5. **td may host, but nothing may require it.** Recipes travel in git; pins
   use upstream-owned URLs plus SHA-256, and the graph must build from those
   URLs alone. td's static release site (`releases.tdlinux.org`) may serve
   two things, both optional and neither a package service nor target input:
   signed whole-system images (`net/RELEASES.md`), and a mirror of pinned
   upstream bytes by digest that fetches try before each pin's URL, so a
   throttled or unreachable upstream does not stop a build (`net/MIRROR.md`).
   `td-feed` is a host cache, and `td-subst` is a consumer for a cache nobody
   is required to operate. Never pin a td-produced artifact or a td-hosted URL.
6. **The recipe graph is the distribution.** Source updates travel in git;
   optional signed image delivery materializes a complete deployment from that
   graph, without changing its target trust roots. System deployments retain
   `current` and `previous` and boot
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

Principle 7 has two explicit exceptions. Trusted live-media installation
needs no secure attention: its isolated wizard and ordinary input are
trusted to confirm the exact reviewed disk and erase/preserve mode, which
is neither installed-session elevation nor authentication
(`td-install/INSTALLER.md`). The designated remote administrator's
separately provisioned SSH key may request signed-release or fixed-main
installation, rollback to the retained deployment and orderly reboot
without another prompt; the source revision is pinned per job, builds use a
separate unprivileged identity, and no caller-supplied command or source is
admitted (`td-update/ADMIN.md`).

Principle 7 is a target, not a current claim; `td-login/THREAT-MODEL.md`,
"How far principle 7 holds", says what holds today.

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

It compares against `origin/main` and includes uncommitted files;
`--path FILE` explains one path's mapping.

# Parallel work

`DEVELOPMENT.md` carries the complete mutation workflow. The always-loaded
boundaries are:

- change work happens in an isolated worktree branch, at the location
  specified in `DEVELOPMENT.md` (for Claude Code, `.claude/worktrees/`);
- agent review copies, continuations, and other disposable scratch belong
  under that worktree's `.td-build-cache/agent-scratch/`, never as loose
  `/tmp` trees. Each agent owns its scratch and deletes it before finishing,
  restoring owner directory permissions when fixtures removed them;
- new or changed test fixtures own their temporary trees with the shared
  `td-fs::TempDir` guard
  (the control-plane workspace includes its source under `cfg(test)` or in
  host-only QEMU oracle code),
  so normal return and assertion unwinding both reclaim them;
- reserve the `-rolling` suffix for a long-lived stacked workstream that
  continues after partial landings; use a descriptive non-rolling branch for a
  one-off change;
- one self-contained, green commit is one independently landable increment;
- never use `git stash` in this repository;
- never signal a process by matching its command line: every worktree runs the
  same binary path, so `pkill -f td-builder` cannot mean "mine". Stop your own
  check run with `td-builder stop`, or signal a pid you recorded yourself;
- `td-builder ready` is the pre-push gate. `--record-only` is not a
  substitute, except after a clean or trivially resolved rebase of a branch
  whose `ready` passed (`DEVELOPMENT.md`, "Rebasing after ready");
- a change request authorizes pushing the ready branch to `origin`; that branch
  is the PR the separate integrator lands.

# Code review

- Each commit gets one independent frontier-model subagent review at high
  effort. Follow `DEVELOPMENT.md` for model selection, the review record
  and confirmation after fixes. Ask the user if passes stop converging.
- The acting agent reconciles every finding and records its own summary,
  the concrete reviewer identity, and checks in the commit message.
- Preserve retained review traces during cleanup. These user-owned audit
  records may be deleted only at the human's explicit request.

A documentation-only commit may use `Review-waiver: docs-only`; `ready`
verifies that every touched path ends in `.md`.

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
- `unsafe` is confined to the syscall surfaces, `td-primitives`' calls into
  CPU instructions a runtime check found, `td-alloc`'s checked heap ownership,
  and the separately recorded
  test-only allocation instrumentation in `UNSAFE.md`. A new
  surface, syscall, value-pinned request, or scoped allow requires an amendment
  there and in any component design/threat model in the same landing. Each
  crate's confinement tests must pin the source-level contract the compiler
  cannot express.
- `builder`, `recipes`, and `engine` are one zero-external-dependency
  workspace. Every other crate is a standalone `td-*` roster crate whose
  lock lists only itself and the `td-*` siblings it declares, each as
  `NAME = { path = "../NAME" }` alone on its line under a bare
  `[dependencies]` or `[dev-dependencies]` header. The only external
  closures are td-net's vendored one and those
  `builder/src/crypto_policy.rs` pins for td-crypto, td-tls-provider and
  td-mta. `DEVELOPMENT.md`, "Crate manifests and locks", carries the full
  manifest, lock and gate-discovery rules and that admission.
- New production buffer/string growth uses `td-alloc` or a reviewed
  allocation-free strategy; exceptions require a specifically reviewed debt
  amendment. Propagate reservation failures without allocating diagnostics.
  `td-alloc/DESIGN.md` owns the source guard, recorded legacy debt, failure
  tests and migration limits.
- Use `std`, not `no_std`. Allocate buffers and collections outside hot loops
  where practical. Keep comments terse and explain a non-obvious why; design
  rationale and review history belong in the commit message or normative doc.
- Declare a constant list as a slice, `const NAMES: &[&str] = &[...]`: two
  branches that each add an entry and bump a `[&str; N]` can merge without
  conflict into a mis-sized array that does not compile. Keep `[T; N]` only
  where the length is itself the invariant or entries are read by position.
  The builder/recipes/engine workspace follows this; convert a `td-*` crate's
  list when touching it.
