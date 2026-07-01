# AGENTS.md — td

You are one of possibly several agents building a functional Linux
distribution.  You grow the OS *inside* a verification loop: you do
not get credit for just code, only for a passing, reproducible test. Read
`DESIGN.md` for the north star, the loop, and the provenance chain; the
parallel-work rules are in this file.

This file is your contract. The rules below are absolute and override any local
convenience.

## North star: full guix independence (human, 2026-06-20; sharpened 2026-06-21)

**The goal is to remove guix entirely — no guix *process* AND no
guix-built *bytes*.** Today the loop runs on a host Guix (the
toolchain seed, the differential oracle, the Guile lowering). The
target end state: td builds and runs its whole userland with **zero
guix bytes in its store** — not in a binary, not even as an embedded
string.

The mechanism is a **full-source bootstrap at `/td/store`**, NOT a
frozen guix-captured seed. A guix-captured seed fails the "no guix
*bytes*" half even when static: a static `bash` still embeds
`/gnu/store` strings (measured: 11) and is guix-*built*; a
`/gnu/store→/td/store` rewrite only relabels guix bytes. So td's
toolchain is **built from source at `/td/store`** from a tiny
auditable seed — the bootstrappable-builds chain (stage0-posix `hex0`
→ `mes` → `mescc-tools` → `tinycc` → `gcc` → `glibc` →
binutils/coreutils/bash/make/…), every stage `--prefix=/td/store`. No
`/gnu/store`, no guix process, no guix bytes. This is a *port* of an
existing reproducible bootstrap (guix's own Full-Source Bootstrap,
live-bootstrap), built **first**, as the foundation the corpus/user-PM
rests on.

The build engine it targets already exists: `td-builder build` stages
inputs and sets `NIX_STORE` at the active `store::store_dir()`, so a
`TD_STORE_DIR=/td/store` build is **native** — re-hashed at
`/td/store`, no post-hoc rewrite. No `guix`
process in any user-facing command/build path (`td shell` resolves
td-built packages, never `guix build`); the `/td/store` source
bootstrap replaces the guix toolchain seed.

## Prime directives (never violate)

1. **Reproducibility is a test.** A non-reproducible build is a
   FAILING test. Never commit a non-reproducible artifact.
   
2. **Hermeticity.** No undeclared dependencies. Builds run offline
   except declared fixed-output fetches. Never make a build pass by
   reaching outside the container, adding an undeclared dependency, or
   disabling `--check`.
   
3. **Never weaken a test silently.** Do not skip, delete, comment out,
   loosen, or `xfail` a test just to turn a task green. It is ok to
   remove tests when migrating to another system (which has its own
   tests). Removing, loosening, or restructuring any existing gate or
   assertion in `check.sh`, the `Makefile`, or `tests/` must be called
   out plainly in the PR so the human approves it knowingly — never
   slip it past review. Adding or strengthening tests is always
   free. If a test cannot pass honestly, STOP and report.
   
4. **PR is the proposal.** One-maintainer project: build the smallest *complete*
   increment — a real working capability with its migration cut over in the one PR
   (directive 9), never a partial mechanism — on a branch and open a PR; the human's
   PR approval is the sign-off. No written proposal or pre-approval is needed to
   start work; build it, then PR it. Keep design notes terse, and surface any
   weakened gate in the PR (directive 3). There is no roadmap to enroll in — the
   open-PR list is the record of what work is in flight, not a gate.

5. **Respect the state boundary.** The VM is ephemeral per test (fresh state, wiped on
   reset) — that is *test isolation*, not a ban on persistence *within* a test.
  
6. **No PR adds a guix dependency** — the guix surface only shrinks
   This generalizes beyond the packager axis to *every* form of guix
   reliance — a `guix` process invocation (`guix build`/`gc`/`repl`/`system`/`shell`), a
   read of guix's private state (e.g. `/var/guix/db/db.sqlite`), or a guix-built byte in
   a td artifact. A new PR MUST NOT introduce one; the existing baselines
   (`tests/guix-surface.expected`, `tests/guix-dependence.expected`, and the `guix gc`/
   `guix repl` census the `guix-surface` gate prints) are one-way ratchets that may only
   shrink. When in doubt, the test is "does the td-native path still work with guix
   deleted from this step?" — if no, it's load-bearing and not allowed.

8. **Every PR is a complete, atomic increment — migrations cut over in
   one PR** A PR delivers a real, working capability, never a partial
   mechanism left for a follow-up. When you replace a path, the SAME
   PR (a) adds the new path, (b) switches every caller onto it, and
   (c) deletes the old path. "Land the engine mechanism now, adopt it
   and remove the old path later" is exactly the split this forbids:
   shipping a new path while the old one stays load-bearing — or
   shipping an unused mechanism, a dead-code path, or a TODO to finish
   the migration — is not done.  This is absolute — there is no "too
   big to cut over in one PR" exception; if a swap feels too big for
   one PR, its scope is wrong (narrow the capability so the whole
   add+cutover+ delete fits), not its atomicity. A migration landed
   half-done is a failing task.  **Scope:** this fires when a PR
   *replaces* an existing path for its *existing consumers* — that add
   + switch-all-callers + delete-old-path is one PR. Building a
   genuinely new capability that nothing consumes yet (a fresh
   bootstrap-ladder rung on the way to retiring guix *last*) is
   additive, not a half-done migration — but it still ships as a
   *complete, behaviorally-tested* capability (see "Test the feature,
   not the possibility"), never an orphan mechanism parked for a later
   adopter. If in doubt, ask for clarification.

## The loop

Run all the tests with the single pass/fail command:

```
./check.sh
```

`./check.sh` is the canonical hermetic entry point: it sets up the
fresh sandbox — **td's OWN `td-builder host-sandbox --expose-cwd`, the
sole loop container** it runs the gate ladder (structural gates
serial-first, heavy gates two at a time), short-circuiting on the
first failure, and exits non-zero on any failure.

Every build/test runs inside that fresh td sandbox so your own
environment cannot contaminate results; `./check.sh <target>` runs a
single Makefile target in the same sandbox.

Do not proceed to the next sub-task until the current one is green.

### Diff-sized local check and waiver

Use the affected-check dispatcher for the fast inner loop and for
local PR readiness when a full run would stall progress. It is the
`affected-checks` subcommand of the Rust engine — `td-builder
affected-checks` (run from the repo root). Resolve `td-builder` to a
prebuilt binary if you have one (`$TD_BUILDER`,
`builder/target/release/td-builder`, or a `td-builder` on PATH), else
build it once: `cargo build --release --manifest-path
builder/Cargo.toml`.

```
td-builder affected-checks        # show changed paths and selected checks
td-builder affected-checks --run  # run the selected preflights and ./check.sh targets
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

If it prints `Waiver: full ./check.sh waived by affected-checks for
this diff`, that is the local waiver for the full loop; record the
selected checks and waiver line in the PR body. If it prints `Waiver:
full ./check.sh required before marking ready`, `--run` executes the
selected checks and then the full `./check.sh` before it can exit
successfully.

**Build-engine changes (`builder/src/*`) are the exception** they no
longer escalate to the full loop — they validate on the
**`check-engine` smoke tier** (`./check.sh check-engine`: a TRUE
~2-min smoke — cheap structural gates + `cargo-test` (compile the
engine + its unit tests), and NOTHING that builds a package from
source) and `affected-checks` waives the full loop for them. The full
heavy+system suite is no longer a per-PR gate; it runs **once daily**
on fresh main via `ci/daily-full-suite.sh`, driven by a scheduled
agent that opens a **fix-or-revert PR (no auto-merge)** on any
regression. A corpus/system regression the smoke misses
is healed within a day, not blocked per-PR — the accepted velocity
trade.


## Test the feature, not the possibility

A new test must exercise an **actual feature through its real entry
point** and assert what that feature *does* — not merely prove an
artifact can be produced. Building an app (or interning a store path)
and asserting its **hash, existence, or shape** shows something is
*possible*; that is not a feature test and does not, on its own, earn
a gate. Drive the real path and assert real behavior: for a shipped
app, run it the way a user does — `td shell <app> -- <app>
--do-some-real-thing` — and check the output; for a mechanism, invoke
it as its real caller would and assert the observable effect.
Build-and-hash, "it interned", "it round-tripped", "the closure is
complete" are the *structural self-consistency* legs above —
legitimate only as SUPPORTING evidence behind a behavioral assertion
(and the byte-hash-vs-Guix leg is the removable oracle), never as the
point of the gate. If the only thing a new test proves is "this can be
built", it is not covering a feature: find the feature and test that.

## Definition of done (every task)

A task is done only when ALL hold:

- a test exercises the actual feature through its real entry point and asserts what it
  does — not just that an artifact can be built (see "Test the feature, not the
  possibility") — and passes,
- you have seen that assertion fail (verified-red) before trusting the pass,
- the change is a complete, atomic increment — a real capability with any replaced path
  removed in the same PR (directive 9), not a partial mechanism,
- it is committed with a message stating what test now passes,
- it is landed on main via the landing protocol below.

If any are missing, the task is not done.

## Parallel work (worktrees, merge on green)

Multiple agents work this repo concurrently. The unit of work is a
**branch + draft PR**. There is no claim file, no roadmap ledger, and no
generated status index — *the open-PR list is the record of who is working
on what*, and all working notes live in the git log + PR body.

- **Claim by opening a draft PR.** Before starting, scan the open PRs
  (`gh pr list`) so two agents don't pick the same work; if one is already open for it,
  pick something else. Open your **draft PR** early (main is branch-protected; nothing
  lands directly) — that draft, with a title naming the workstream, IS your claim.
  Release it by closing the PR if you abandon the work.

- **Work in your own git worktree/branch** (`git worktree add
  ../td-<name>`), never on a shared checkout of main. Your running
  notes, sub-task ladder, and verified-red evidence go in your
  **commit messages and the PR body** — never in a tracked file. Do
  not create files to track or claim work; tracking and claiming are
  the open PRs, full stop.

- **Land (optimistic merge on green, via PR):** main is **non-strict** — a PR merges on **its own** green checks; main moving
  under you no longer forces a rebase-onto-tip + re-run. So: (1) validate against
  your own base — run `td-builder affected-checks --committed-only --run`; if it
  waives the full loop, record the waiver in the PR body; if it escalates, it
  runs the FULL `./check.sh` before returning success, so record the escalation
  and full result instead; (2) spawn an independent code-review subagent over the
  full branch diff (`/code-review`) and address its findings (Workflow step 6 —
  MANDATORY for AI agents), then push the branch and mark the PR
  ready — CI runs
  the required hosted gate and a human review approves (main is branch-protected:
  required checks + mandatory review, no direct pushes —
  `.github/BRANCH-PROTECTION.md`); (3) merge once green and approved — default to
  arming auto-merge (`gh pr merge --auto --squash`; squash is the only merge mode
  enabled) so the human's
  approval is the last manual step; merge manually instead when the landing must
  be sequenced (e.g. exclusive landings stacked behind another PR).
  **Do NOT rebase-onto-tip + re-run just because main moved** — that is the toil
  we deliberately dropped. Rebase only when GitHub reports a real git conflict
  (or for an exclusive-landing sequence). The rare broken combination
  (green(A)+green(B) ≠ green(A∪B)) is healed by an agent, not a bot:
  **whenever you fetch main — to start work or to land — check its latest
  `check-fast`; if it is red, run `ci/revert-suspect.sh --open-pr` to open a
  revert PR for the suspect squash commit (main's HEAD) before continuing.**
  Squash makes the suspect atomic; the script's loop guard refuses to revert a
  revert. There is no automated revert workflow — the duty is the next agent's
  (check with `gh run list --branch main --workflow ci.yml -L1` or
  `gh api repos/<owner>/td/commits/main/check-runs`). A heavy-only break
  (boot/VM/repro, not seen by the fast tier) is NOT caught by check-fast either —
  it surfaces on the next manual full `./check.sh`; this is an accepted gap of
  the velocity trade. Marking a PR ready
  with a locally-red or un-run affected-checks gate, or without the full run when
  affected-checks escalates, is still a contract violation — CI verifies your
  run, it does not replace it. `lint` + `check-fast` are the required checks; the
  full `./check.sh` stays the dev-machine gate (step 1).
  
- **Exclusive landings:** changes to the shared spine — `system/td.scm` (frozen
  oracle), `check.sh`, `Makefile`, `channels.scm`, `DIGESTS.md` — collide with
  everyone. Announce in the PR description, land as small standalone PRs, expect
  others to rebase. Oracle re-baselines and channel bumps are the canonical cases.
  Note: **adding a gate is no longer an exclusive landing** — it's a new
  `mk/gates/<NNN>-<name>.mk` file, not a `Makefile` edit, so concurrent gate PRs
  don't collide (the core `Makefile` itself stays exclusive).
  
- **Resources:** each full check already runs its heavy gates two at a time (`-j2`),
  so two concurrent checks mean up to four VMs/builds — the observed ceiling. Don't
  add a third check or raise `-j`; stagger if the host is loaded.

## Repo conventions

**Directory layout**

- `check.sh` — the canonical hermetic, offline pass/fail command (`./check.sh`). The
  only command you need to determine green/red.
- `Makefile` — the `make check` target it runs inside that sandbox; it assembles the
  `CHEAP_GATES`/`HEAVY_GATES` pools from the `mk/gates/*.mk` drop-in fragments and
  derives `.PHONY`, the `check` targets, and the ordering graph from them.
- `mk/gates/` — one drop-in fragment per gate (`<NNN>-<name>.mk`: a `CHEAP_GATES`/
  `HEAVY_GATES +=` self-registration line, the recipe, and its doc comment). This
  directory IS the authoritative gate list — adding a gate adds a file, so concurrent
  gate PRs touch different files and don't collide on a shared list line. The `<NNN>`
  prefix sets order (cheap serial-first, heavy LPT for `-j2`); `make list-gates` prints
  the assembled pools.
- `system/` — Guile system declarations. The frozen oracle lives at `system/td.scm`.
- `tests/` — the package-manager/differential/coverage gate scripts and the
  `guix system image` / OCI / placement gates. (The marionette `(gnu tests)`
  VM-boot tests were removed 2026-06-29, human-directed.)
- `channels.scm` — pinned Guix channel commit. Reproducibility is anchored here; bump it
  deliberately (exclusive landing), never silently.
- `DESIGN.md` — the settled north star: scope (§0), the loop (§1), and the
  provenance chain. The parallel-work rules live here in AGENTS.md, not DESIGN.md.
- Work tracking: there is **no `PLAN.md` and no per-PR tracking/claim files** — claims
  are the open draft PRs (`gh pr list`), and work notes + verified-red evidence live in
  commit messages + the PR body (the squash merge preserves the commit messages in
  `git log`).
- `td-builder affected-checks` (`builder/src/affected.rs`) — diff-to-check
  dispatcher for local iteration and PR readiness. Run it first to see the selected
  checks, then `td-builder affected-checks --run` to execute them. Its waiver line
  decides whether the local full `./check.sh` run is waived or still required;
  `--run` enforces escalation by running the full loop when required. Its own mapping
  guard is `td-builder affected-checks --self-test` (also a `cargo-test` `#[test]`).
- `HISTORY.md` — completed-milestone record. `DIGESTS.md` — reproducibility record
  (changes only on oracle re-baseline; exclusive landing).

**Naming & formatting**

- Scheme files: lowercase kebab-case (`td.scm`, `eval.scm`). Modules carry a `td`
  prefix.
- Hand-formatted 2-space indentation, no tabs. Do NOT run `guix style` — it was tried
  in M2 and mangled the layout; the hand-formatted style is the convention.
- Run every build/test via `./check.sh` (or `./check.sh <target>`), which enters td's
  own `td-builder host-sandbox` for you (see "The loop"). Don't add `--network` to
  it — that pulls substitutes (offline/hermeticity violation).


**Commits**
  
- **Commit messages ARE the durable record.** main takes only squash merges (merge and
  rebase merges are disabled), and the repo composes the squash commit's body from your
  branch's commit messages, not the PR description (`squash_merge_commit_message =
  COMMIT_MESSAGES`). So put the rationale, the design decisions, and the verified-red
  evidence in your commit messages — that is what lands in `git log` on main. The PR
  body is review context for the human and does NOT persist into git; don't rely on it
  to record anything you want to keep.
