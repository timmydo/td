# AGENTS.md — td

You are one of possibly several agents building a functional Linux
distribution.

# Build

The root seed is brought by the host builder in the form of a rust
toolchain. From there we build the td build tools (td builder, td
fetch, etc.), which are used to bootstrap the rest of the distro.

We start with a mes bootstrap. The units are called recipies. They are
built like Nix/Guix to `/td/store`. So td's toolchain is **built from
source at `/td/store`** from a tiny auditable seed — the
bootstrappable-builds chain (stage0-posix `hex0` → `mes` →
`mescc-tools` → `tinycc` → `gcc` → `glibc` →
binutils/coreutils/bash/make/…), every stage `--prefix=/td/store`.

The build engine it targets already exists: `td-builder build` stages
inputs and sets `NIX_STORE` at the active `store::store_dir()`, so a
`TD_STORE_DIR=/td/store` build is **native** — re-hashed at
`/td/store`, no post-hoc rewrite.

The rust toolchain td uses to build rust packages is NOT part of the
full-source-bootstrap requirement. It enters as a **pinned upstream
rust release** (a declared fixed-output fetch) transformed by a td
**recipe** that rewrites the ELF binaries (PT_INTERP/RUNPATH via
`builder/src/elf.rs::set_interp`) onto td's own `/td/store` glibc.

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
  past its one self-entry. Two crate tiers are allowed dependencies: the network
  tools (`fetch`/`feed`/`subst`) with the vendored-through-the-cargo-proxy FSDG
  set they already have (`ureq`/`rustls`/`sha2`/`ring`), and the seed shell
  (`sh/` → `td-sh`), a thin wrapper over `brush-shell` (the pure-Rust MIT
  bash-compatible shell) so bootstrap rungs can declare a td shell instead of a
  host bash (re #469) — pinned to brush-shell + its checksum-locked closure;
  any `sh/Cargo.lock` change (a brush bump most of all) is the same
  reviewed-decision territory, and before any rung declares td-sh its closure
  must ride the same td-fetch warm/vendor offline path the network tools use
  (a live crates.io resolution is not a declared fixed-output fetch). A *new*
  dependency anywhere is a reviewed decision (directive 6 territory), never
  casual.
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
