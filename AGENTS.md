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
The `td-builder` executable that implements a derivation is staged and
executed with explicit `ControlPlaneBuilder` provenance solely to run
that build. This typed exception does not make it a recipe tool, target
artifact, or runtime dependency, and no other host-built program may be
exposed to recipe steps.

The host Rust toolchain is therefore a control-plane seed, not the
distribution's bootstrap seed. After td has a target Rust toolchain,
the shipped copies of td's own programs are rebuilt as target recipes;
host-built control-plane binaries do not enter the final image.

## Target artifact graph

The target distribution begins with the tiny, auditable stage0-posix
seed. Recipes build the artifact graph directly into `/td/store`:

```text
stage0-posix (hex0/kaem lineage)
  -> Mes/MesCC
  -> TinyCC
  -> early Make/Patch/Bash
  -> iterative binutils/GCC/glibc bootstrap
  -> native binutils/GCC/glibc and GNU build userland
  -> transformed upstream Rust bootstrap snapshot
  -> source-built stage1 Rust compiler and in-tree standard library
  -> full-bootstrap source-built stage2 Rust toolchain
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

Rust enters only after the native GCC/glibc build platform and its GNU
build userland exist. td pins both a Rust source release and the exact
upstream bootstrap snapshot required by that source; "latest" is never
resolved ambiently, and changing either pin is a reviewed update.

The bootstrap snapshot contains rustc, Cargo, rust-std, the compiler
sysroot, and compiler runtime libraries such as its prebuilt LLVM. It
is a declared fixed-output source transformed by a td recipe. The
recipe unpacks the snapshot with the dependency-free `td-builder`,
rewrites rustc and Cargo's ELF interpreters with td's in-process ELF
editor, and supplies the declared td-built runtime closure (including
glibc, libgcc_s, and zlib). The result is a normal content-addressed
`/td/store` recipe output used only as Rust stage0.

The retargeted stage0 compiler builds compiler artifacts from the
pinned Rust source and assembles them as stage1. Stage1 builds the
in-tree standard library, then rebuilds the compiler against that
source-built library as stage2. td enables Rust's full-bootstrap mode:
stage2 rebuilds the final rust-std rather than uplifting the stage1
library, and in-tree Cargo is built with the source-built toolchain.
Those stage2 rustc, rust-std, and Cargo outputs form td's shipped
`/td/store` Rust toolchain; no downloaded Cargo, library, or other
stage0 byte enters a final distribution closure.

The Rust source tree, its Cargo source closure, LLVM source, and every
native build tool are declared inputs built or fetched under the same
recipe rules. Using a prebuilt LLVM in the shipped stage2 toolchain
would be a separate binary trust exception and requires explicit
sign-off; the prebuilt LLVM used by the bootstrap snapshot is already
part of the stage0 trust root and is excluded from final closures.

The entire downloaded stage0 closure remains an explicit bootstrap
trust root, including its compiler, Cargo, standard library, compiler
runtime, and LLVM libraries. Rebuilding stage2 from source improves
artifact provenance but does not by itself defeat a trusting-trust
compiler. A stronger claim requires a separately specified diverse
bootstrap or diverse-double-compilation proof.

Both the bootstrap snapshot and source-built toolchain must run without
host `/bin`, `/usr`, or libraries and resolve every dynamic dependency
from their declared closures. The Rust bridge test builds stage2,
asserts that full-bootstrap did not uplift stage1 or copy any stage0
bytes, and uses stage2 to compile, link, and run a program against td's
native toolchain and glibc. An optional stage3 rebuild is the
same-result backstop.

## Rust dependencies and final userland

Rust dependency sources are fixed-output inputs, not ambient network
access. Registry crates are selected by committed `Cargo.lock` entries
and verified against their checksums. Git dependencies are not
currently supported; introducing one requires explicit dependency
sign-off and a fixed-output representation pinned by commit and archive
hash before any build may use it. Fetching may populate the supported
source closure before the Rust compiler exists. Rust's own pinned
compiler and standard-library sources are compiled during the staged
Rust bootstrap; compilation of td tools and distribution packages
happens only after the source-built stage2 Rust toolchain is available.

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
be Rust built with the source-built stage2 `/td/store` toolchain.

# Principles

1. No undeclared dependencies. Builds run offline except declared
   fixed-output fetches. Do not make a build pass by reaching outside
   the container or adding an undeclared dependency
   
2. Avoid external dependencies. Request explicit sign off before
   adding and make it clear in the landing commit message if this adds
   a new dependency.

3. Avoid writing shell. Prefer rust code with zero dependencies.

4. Treat migrations as a complete, atomic increment — a migration cuts
   over in one landing. Delete the old mechanism in the same landing.
   We use git. You don't need to put dates in annotations--git blame is
   for that.
   
# Tests

Run all the tests with the single pass/fail command:

```
cargo run --release --manifest-path builder/Cargo.toml -- check
```

Recipes should have tests that test the output.

We have different check tiers (daily, per-change, etc.). It'd take too
long to rebuild the world from scratch for every change so we only run
the minimal per change; the daily backstop covers the deep tiers.

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
selected checks and waiver line in the landing commit message
(include the "Deferred to the daily backstop" line when one prints —
that is the record of what the daily covers for this diff). Nothing
escalates to the full loop; every diff waives to the bounded per-change
tiers + the daily backstop.

# Parallel work (worktrees, land on green)

Multiple agents work this repo concurrently so use work trees. GitHub
(and the sr.ht mirror) is a git backup remote only — no GitHub Issues,
PRs, Actions, or branch protection.

Work in your own git worktree/branch named `work-NNNN-slug`.

**Ready.** A branch is ready to land when its bounded checks are green
(`td-builder affected-checks --committed-only --run`) AND all three
code reviews have run, their findings acted on, and the acting agent's
summary of those findings and follow-ups is in the commit message.
There is no draft/ready flag to flip and nothing on a webpage to ask
the human to look at — readiness lives entirely in the branch. The
SAME agent that finishes the work carries it to ready; don't hand a
half-reviewed branch to the integrator.

**Land.** A single integrator (the test user) lands ready branches into
main: `git fetch`, `git squash-in <branch>` (squashes the branch,
prefilling the message from its commits), review `git diff --cached`,
`git commit`, `git push origin main`. 

Never `git stash` in this repo. The stash stack (`refs/stash`) is
  repo-*global*.

## Code review — three independent reviews, recorded in the commit

Every landing gets THREE independent code reviews — a subagent review
AND two cross-model reviews, each by a different model's CLI — waivable
only for documentation changes, and only if the commit message says so.
Spawn an independent code-review subagent over the full branch diff
(`/code-review`), AND run two further reviews with *different* models
driven from their CLIs so two distinct models each audit the same diff
(catches blind spots one model shares with its own subagent). Which
three reviewer identities apply depends on which model is the acting
agent:

- **Acting agent is Claude:** subagent review at Opus 4.8, plus a
  Codex CLI review and an Agy (Antigravity) CLI review.
- **Acting agent is Codex:** subagent review at gpt-5.6-sol, plus a
  Claude CLI review and an Agy CLI review.

Run every cross-model reviewer at a strong model + high reasoning
effort, and feed it the branch diff on stdin — a bare `git diff` is
empty once the branch is committed, so pipe `git diff
origin/main...HEAD` so each reviewer audits the same full branch diff
the subagent does. The reviewers do NOT write the record: the acting
agent reads all three, acts on the findings — fixing each real one or
dismissing it with a stated reason — then writes ITS OWN summary of the
findings and how each was followed up into the commit message. Send raw
reviewer output to a scratch file you do NOT commit; account for every
finding (don't silently drop one), but the commit carries your summary,
not the verbatim dumps. That summary is the durable record the
integrator reads before `squash-in`.

Claude runs Codex (gpt-5.6-sol, xhigh):

```
git diff origin/main...HEAD | codex exec --model gpt-5.6-sol -c model_reasoning_effort="xhigh" -s read-only --ephemeral "Do a code review of the git diff on stdin. Do not edit files. Return prioritized findings with file/line references where possible." | tee /tmp/codex-review.md
```

Codex runs Claude (Opus 4.8, xhigh):
```
git diff origin/main...HEAD | claude -p --model opus --effort xhigh "Do a code review of the git diff on stdin. Do not edit files. Return prioritized findings with file/line references where possible." | tee /tmp/claude-review.md
``` 

Either acting agent also runs Antigravity (Gemini 3.1 Pro High) as the
third, shared cross-model reviewer:

```
git diff origin/main...HEAD | agy --model "Gemini 3.1 Pro (High)" --mode plan --print-timeout 10m --print "Do a code review of the git diff on stdin. Do not edit files. Return prioritized findings with file/line references where possible." | tee /tmp/agy-review.md
```

Use `--model`/`--effort` for `claude` (effort level `xhigh`); `--model`
+ `-c model_reasoning_effort=…` for `codex`; and `--model`/`--mode
plan` for `agy`. Summarize the three reviews' findings and your
follow-ups into the landing commit message — your summary, not the raw
reviewer output.


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
- **Code comments are terse.** A comment earns its place by explaining
  a non-obvious *why* in a line or two — not by narrating the change,
  restating the code, review history, or design rationale. That
  context belongs in the commit message (`git blame` walks any line
  back to it); the review reconciliation belongs there too. Match the
  surrounding comment density; when in doubt, cut.


**Commits**
  
- **Commit messages ARE the durable record.** main is built from squash landings
  (`git squash-in` composes the squashed commit's body from your branch's commit
  messages). So put the rationale, the design decisions, the review findings +
  resolutions, and the verified-red evidence in your commit messages — that is what
  lands in `git log` on main and what the integrator reads before landing. Nothing
  else persists (there is no PR description, no webpage); if you want to keep it, it
  goes in a commit message.

