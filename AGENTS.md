# AGENTS.md — td

You are one of possibly several agents building a functional Linux
distribution.

# Build and trust model

td has two bootstrap graphs. Do not confuse the host control plane
with the target distribution artifact graph.

## Control plane

The host builder supplies a pinned Rust toolchain used to compile td's
control-plane programs (`td-builder`, `td-recipe-eval`, `td-fetch`,
and related tools). These programs may evaluate recipes, fetch
declared fixed-output sources, construct sandboxes, and place outputs.
The derivation builder itself is staged and executed with explicit
`ControlPlaneBuilder` provenance solely to implement the build. That
typed exception does not make it a recipe tool, target artifact, or
runtime dependency, and no other host-built program may be exposed to
recipe steps.

The host Rust toolchain is therefore a control-plane seed, not the
distribution's bootstrap seed. After td has a target Rust toolchain,
the shipped copies of td's own programs are rebuilt as target recipes;
host-built control-plane binaries do not enter the final image.

## Target artifact graph

This section is the normative target architecture. Transitional
mechanisms still present in the tree are migration state, not
architectural direction; issues and PRs record their current status.

The target distribution begins with the tiny, auditable stage0-posix
seed. Recipes build the artifact graph directly into `/td/store`:

```text
stage0-posix (hex0/kaem lineage)
  -> Mes/MesCC
  -> TinyCC
  -> early Make/Patch/Bash
  -> iterative binutils/GCC/glibc bootstrap
  -> native binutils/GCC/glibc and GNU build userland
  -> transformed upstream Rust toolchain
  -> target-built td tools and Rust userland
  -> uutils-based distribution closure and image
```

The GCC/binutils/glibc portion is an iterative ladder, not a single
linear build: early compiler and libc generations build later,
increasingly native generations until the native toolchain can rebuild
itself. The bootstrap uses source-built GNU userland packages,
including coreutils, sed, grep, gawk, findutils, diffutils, Bash, Make,
and the required archive/compression tools. They remain available as
declared recipe outputs until the Rust toolchain and Rust userland have
been built.

Recipe steps may execute only audited seed executables, outputs of
earlier recipes, and executables created by the current build. The
typed control-plane builder is the sole sandbox exception and executes
only as the derivation implementation, never from a recipe's `PATH` or
argv. Host `/bin`, `/usr`, ambient `PATH`, and arbitrary host store
paths are never target inputs.

`td-builder build` stages those declared inputs and sets the
compatibility `NIX_STORE` variable to the active td store directory;
this does not introduce a Nix dependency. A
`TD_STORE_DIR=/td/store` build is native: it is hashed for and built at
`/td/store`, with no post-hoc store-prefix rewrite.

## Rust bridge

Bootstrapping Rust from source is not currently part of td's
full-source-bootstrap requirement. Rust enters only after the native
GCC/glibc build platform and its GNU build userland exist.

The Rust seed is a pinned upstream release containing rustc, Cargo,
rust-std, and the compiler sysroot. It is a declared fixed-output
source transformed by a td recipe. The recipe unpacks the release with
the dependency-free `td-builder`, rewrites rustc and Cargo's ELF
interpreter with td's in-process ELF editor, and supplies the declared
td-built runtime closure (including glibc, libgcc_s, and zlib). The
result is a normal content-addressed `/td/store` recipe output. The
upstream compiler and prebuilt standard library remain an explicit
binary trust root.

The transformed toolchain must run without host `/bin`, `/usr`, or
libraries; resolve every dynamic dependency from its declared closure;
and compile, link, and run a program against td's native toolchain and
glibc.

## Rust dependencies and final userland

Rust dependency sources are fixed-output inputs, not ambient network
access. Registry crates are selected by committed `Cargo.lock` entries
and verified against their checksums. Git dependencies are not
currently supported; introducing one requires explicit dependency
sign-off and a fixed-output representation pinned by commit and archive
hash before any build may use it. Fetching may populate the supported
source closure before the Rust compiler exists, but compilation happens
only after the transformed Rust toolchain is available.

Cargo builds run offline. Build scripts, proc macros, and crates that
contain C or assembly are part of the declared build graph and may use
only td-built tools. Native crate code is compiled by td's GCC/binutils
and links against td's runtime closure, never a host compiler or host
library.

After the Rust bridge, td is Rust-first. The final distribution image
uses uutils for its core userland rather than carrying the GNU
bootstrap userland forward. The source-bootstrap toolchain, glibc,
Linux kernel, boot/firmware components, and explicitly reviewed
non-Rust packages are exceptions; new td-owned shipped userland should
be Rust built with the transformed `/td/store` toolchain.

# Principles

1. No undeclared dependencies. Builds run offline except declared
   fixed-output fetches. Do not make a build pass by reaching outside
   the container or adding an undeclared dependency
   
2. Issues track the work. PRs claim the issues and save the
   state. Open a draft PR when you start working, commit often (we
   squash merge PRs) so that other agents can pick up the work in the
   case of an unexpected interruption (e.g. reboot).

3. Avoid external dependencies. Request explicit sign off before
   adding and make it clear in the PR if this adds a new dependency.

4. Avoid writing shell. Prefer rust code with zero dependencies.

5. Treat PR migrations as a complete, atomic increment — migrations
   cut over in one PR. Delete the old mechanism in the same PR. We use
   git. You don't need to put dates in annotations--git blame is for
   that.
   
# Tests

Run all the tests with the single pass/fail command:

```
cargo run --release --manifest-path builder/Cargo.toml -- check
```

Recipes should have tests that test the output.

We have different CI tiers (daily, PR, etc.). It'd take too long to
rebuild the world from scratch for every PR so we only run the minimal
during PRs.

Build td builder with: `cargo build --release --manifest-path builder/Cargo.toml`.

```
td-builder affected-checks        # show changed paths and selected checks
td-builder affected-checks --run  # run the selected preflights and check targets
```

It compares the branch to `origin/main` (falling back to `main`) and
includes dirty, staged, and untracked files by default. Use
`--committed-only` before push/PR review when you want the committed
branch diff only, or `--path FILE` to inspect the mapping for a
specific file.

After rebasing for PR readiness, run:

```
td-builder affected-checks --committed-only --run
```

It prints `Waiver: the full check waived by affected-checks for
this diff` — the local waiver for the full loop; record the
selected checks and waiver line in the PR body (include the
"Deferred to the daily backstop" line when one prints — that is the
record of what the daily covers for this diff). Nothing escalates to
the full loop; every diff waives to the bounded per-PR tiers + the
daily backstop.

# Parallel work (worktrees, merge on green)

Multiple agents work this repo concurrently so use work trees. Take
new work from the issue backlog. `gh issue list` is the menu.  Draft
means "not yet reviewable" — nothing else. A draft PR is a CLAIM; the
moment its validation + review protocol is complete, the SAME agent
marks it ready — never ask the human to look at a PR still labeled
draft, and never leave a finished PR in draft (the human reads draft
as "don't review yet").  If the human asks about a draft PR, treat it
as the signal your readiness state drifted: reconcile immediately
(mark ready or say what's missing).  Marking ready REQUIRES both code
reviews (the subagent review AND the cross-model CLI review) to
already be POSTED on the PR with their findings addressed

Work in your own git worktree/branch.

Never `git stash` in this repo. The stash stack (`refs/stash`) is
  repo-*global*.

Every PR gets TWO independent code reviews — the subagent review AND a
cross-model review by a different model's CLI — both waivable only for
a trivial docs- or comment-only diff, and only if you say so in the
PR:** spawn an independent code-review subagent over the full branch
diff (`/code-review`), AND run a second review with a *different*
model driven from its CLI so a distinct model audits the same diff
(catches blind spots one model shares with its own subagent). Run the
cross-model reviewer at a strong model + high reasoning effort, and
feed it the branch diff on stdin — a bare `git diff` is empty once the
branch is committed, so pipe `git diff origin/main...HEAD` so the
cross-model reviewer audits the same full branch diff the subagent
does. Subagents MUST post their review findings to the PR as a comment
(unfiltered by the main agent requesting it).

Claude runs Codex (gpt-5.5, xhigh): 

```
git diff origin/main...HEAD | codex exec --model gpt-5.5 -c model_reasoning_effort="xhigh" -s read-only --ephemeral "Do a code review of the git diff on stdin. Do not edit files. Return prioritized findings with file/line references where possible. Post the review as comment on PR #<insert number>"
```

and Codex runs Claude (Opus 4.8, xhigh): 
```
git diff origin/main...HEAD | claude -p --model opus --effort xhigh "Do a code review of the git diff on stdin. Do not edit files. Return prioritized findings with file/line references where possible. Post the review as comment on PR #<insert number>"
``` 

Codex can also run Antigravity (Gemini 3.1 Pro High) as a distinct
cross-model CLI reviewer. `agy --print` reads stdin but does not post
to GitHub itself in plan mode, so post its raw output unchanged:

```
git diff origin/main...HEAD | agy --model "Gemini 3.1 Pro (High)" --mode plan --print-timeout 10m --print "Do a code review of the git diff on stdin. Do not edit files. Return prioritized findings with file/line references where possible." | tee /tmp/agy-review.md
gh pr comment <insert number> --body-file /tmp/agy-review.md
```

Use (`--model`/`--effort` for `claude` — effort levels `xhigh`;
`--model` + `-c model_reasoning_effort=…` for `codex`, and
`--model`/`--mode plan` for `agy`. Use `gpt-5.5`, not
`gpt-5.5-codex`, under a ChatGPT-account login) `codex` is installed
and on `$PATH` — do NOT drop it for a second `claude` CLI run, which
varies only the model, not the harness.


# Rust code

td's Rust is defensive and minimal-surface.

- **No panics on the happy or error path.** No `unwrap()`, `expect()`, `panic!`,
  `unreachable!`, `todo!`, or `unimplemented!`. Return `Result`/`Option` and
  propagate with `?`. (Inline `#[cfg(test)]` code may `unwrap` — clippy does not
  lint it.)
- **`.get(i)` over `xs[i]`.** No indexing/slicing that can panic (`clippy::indexing_slicing`).
- **`unsafe` is confined.** The only `unsafe` is the raw-syscall layer
  (`builder/src/sys.rs` and its callers `nar.rs`/`sandbox.rs`), which carry
  `#![allow(unsafe_code)]` so `builder` can be `libc`-free. Every other crate
  `forbid`s `unsafe_code`. Do not add `unsafe` anywhere else.
- **The engine is dependency-free.** `builder` and `recipes` carry **zero crates**
  (pure `std`) and must stay that way — the gate fails if either `Cargo.lock` grows
  past its one self-entry. The network tools (`fetch`/`feed`/`subst`) are the *only*
  crates allowed dependencies, and only the vendored-through-the-cargo-proxy FSDG
  set they already have (`ureq`/`rustls`/`sha2`/`ring`); a *new* dependency anywhere
  is a reviewed decision (directive 6 territory), never casual.
- **`std`, not `no_std`.** These are OS-driving userspace programs
  (`std::fs`/`std::process`/namespace syscalls); `no_std` is out of scope.
- **Prefer allocating off the hot path** — set buffers/collections up once rather
  than per-iteration in a build's inner loop. This is a code-review guideline, not
  a lint (there is no clippy check for it); don't contort code to satisfy it.


**Commits**
  
- **Commit messages ARE the durable record.** main takes only squash merges (merge and
  rebase merges are disabled), and the repo composes the squash commit's body from your
  branch's commit messages, not the PR description (`squash_merge_commit_message =
  COMMIT_MESSAGES`). So put the rationale, the design decisions, and the verified-red
  evidence in your commit messages — that is what lands in `git log` on main. The PR
  body is review context for the human and does NOT persist into git; don't rely on it
  to record anything you want to keep.

- **Closing keywords in commit messages fire on main.** Because the squash body is
  composed from branch commit messages, a `fixes #N` / `closes #N` / `resolves #N`
  ANYWHERE in any branch commit auto-closes that issue the moment the squash lands —
  GitHub offers no setting to disable this, and it has mis-closed a live issue before
  (#292, closed by the unrelated #291 squash whose body said "for whoever fixes #292").
  Write `re #N` / `see #N` / `until #N is fixed` when referring to an issue you are NOT
  resolving; reserve the closing keywords for the issue your PR actually closes.
