# CLAUDE.md — td

You are one of possibly several agents building a functional Linux distribution
incrementally on top of an existing Guix system. You grow the OS *inside* a
verification loop: you do not get credit for code, only for a passing, reproducible
test. Read `DESIGN.md` for the target, the roadmap (§7.1 — a descriptive status
index, not a pre-approval gate), and the parallel-work rules (§7.2–7.4).

This file is your contract. The rules below are absolute and override any local
convenience.

## North star: full guix independence (human, 2026-06-20; sharpened 2026-06-21)

**The goal is to remove guix entirely — no guix *process* AND no guix-built *bytes*.**
Today the loop runs on a host Guix (the toolchain seed, the differential oracle, the
Guile lowering). The target end state: td builds and runs its whole userland with
**zero guix bytes in its store** — not in a binary, not even as an embedded string.

The mechanism is a **full-source bootstrap at `/td/store`**, NOT a frozen guix-captured
seed *(2026-06-21, human — supersedes the 2026-06-20 "binary seed tarball")*. A
guix-captured seed fails the "no guix *bytes*" half even when static: a static `bash`
still embeds `/gnu/store` strings (measured: 11) and is guix-*built*; a
`/gnu/store→/td/store` rewrite only relabels guix bytes. So td's toolchain is **built
from source at `/td/store`** from a tiny auditable seed — the bootstrappable-builds
chain (stage0-posix `hex0` → `mes` → `mescc-tools` → `tinycc` → `gcc` → `glibc` →
binutils/coreutils/bash/make/…), every stage `--prefix=/td/store`. No `/gnu/store`, no
guix process, no guix bytes. This is a *port* of an existing reproducible bootstrap
(guix's own Full-Source Bootstrap, live-bootstrap), built **first**, as the foundation
the corpus/user-PM rests on.

The build engine it targets already exists: `td-builder build` stages inputs and sets
`NIX_STORE` at the active `store::store_dir()`, so a `TD_STORE_DIR=/td/store` build is
**native** — re-hashed at `/td/store`, no post-hoc rewrite (user-pm Phase 1/3). guix may
still appear ONLY as a removable differential oracle (build the same source both ways,
diff — directive 4), never as a build input. Priority order: (1) no `guix` process in any
user-facing command/build path (`td shell` resolves td-built packages, never `guix
build`) — DONE for the shell; (2) the `/td/store` source bootstrap replaces the guix
toolchain seed; (3) the loop's oracle/lowering (`guix build --check`, `guix repl`/`system`)
retired last. DESIGN §5 carries the detail.

## Prime directives (never violate)

1. **Reproducibility is a test.** Every artifact must pass `guix build --check`. A
   non-reproducible build is a FAILING test. Never commit a non-reproducible artifact.
2. **Hermeticity.** No undeclared dependencies. Builds run offline except declared
   fixed-output fetches. Never make a build pass by reaching outside the container,
   adding an undeclared dependency, or disabling `--check`.
3. **Never weaken a test silently.** Do not skip, delete, comment out, loosen, or
   `xfail` a test just to turn a task green. Removing, loosening, or restructuring any
   existing gate or assertion in `check.sh`, the `Makefile`, or `tests/` must be called
   out plainly in the PR so the human approves it knowingly (DESIGN §4.3) — never slip
   it past review. Adding or strengthening tests is always free. If a test cannot pass
   honestly, STOP and report.
4. **Differential testing before replacement.** Never replace a Guix component
   (`guix-daemon`, store, config language, etc.) without first proving behavioral
   equivalence to the original — build the same thing both ways and diff the store
   paths. The existing component is the oracle.
5. **PR is the proposal.** One-maintainer project: build the smallest correct
   increment on a branch and open a PR — the human's PR approval is the sign-off
   (DESIGN §4.3). No roadmap entry, written proposal, or pre-approval is needed to
   start work; build it, then PR it. Keep design notes terse, and surface any
   weakened gate in the PR (directive 3). The roadmap (DESIGN §7.1) is a descriptive
   status index, not a gate.
6. **Respect the state boundary.** The VM is ephemeral per test (fresh state, wiped on
   reset) — that is *test isolation*, not a ban on persistence *within* a test:
   a guest that reboots mid-test and keeps its placed generations (M10) is legitimate
   behavior under test. `/gnu/store` and the system declaration are immutable. What
   may persist on a machine is default-deny and declared (DESIGN §2.6): only
   allowlisted paths on the `td-state` filesystem survive a generation swap; machine
   identity (SSH host keys) lives there, never in a generation's root. Never stash
   mutable state outside the declared boundary to make something work.
7. **No new guix-as-packager surface (move-off-Guile §5).** A new tool/seed is
   provisioned td-native — a pinned fixed-output fetch the loop realizes + td's own
   placement (`store-add-recursive`) — never a guix `(build-system …)` package built by
   resolving `(@ (system M) pkg)` via `guix build -e` (or `specification->package`).
   Adding one is a move-off-Guile regression: it requires a `tests/guix-surface.expected`
   entry, called out in the PR for sign-off (directive 3). The `guix-surface` gate
   ratchets this surface — it may only shrink. The existing seed packages
   (`td-builder`/`td-ts-eval`/`td-typescript`) are the snapshotted baseline, retired by
   their own tracks; this just stops NEW ones (author in TS, build/place with td).

## The loop

Your inner loop for every change:

```
write/change declaration
  -> evaluate config            (fails fast, sub-second)
  -> guix build --check TARGET  (reproducibility oracle)
  -> boot marionette system test (boot + behavioral assertion)
  -> reset VM state
```

Run all of it with the single pass/fail command:

```
./check.sh
```

`./check.sh` is the canonical hermetic entry point: it sets up the fresh sandbox —
**td's OWN `td-builder host-sandbox --expose-cwd`, the sole loop container** (no
`guix shell -C` fallback, no toggle — DESIGN §7.1): store/cache/daemon-socket exposure,
host guix + the toolchain on PATH, a private PID namespace + `/proc`, its own
loopback-only netns, **substitutes disabled** so the loop is offline. It guards that the
host guix matches the `channels.scm` pin, then runs `make check` inside it. `make check`
is the underlying target (assumes you are already in that sandbox); it runs the gate
ladder (structural gates serial-first, heavy gates two at a time), short-circuiting on
the first failure, and exits non-zero on any failure.
The gate list is assembled from the drop-in fragments under `mk/gates/*.mk`: each
fragment registers itself into the `CHEAP_GATES`/`HEAVY_GATES` pool that the `check:`
target expands. That directory is the **authoritative gate list**, the one place it is
written down — add a gate by dropping a new `mk/gates/<NNN>-<name>.mk` file, never by
editing a shared list line (so concurrent gate PRs don't collide). Broad shape:
config eval →
differentials → `guix build --check` → behavioral/marionette tests.

Every build/test runs inside that fresh td sandbox so your own environment cannot
contaminate results; `./check.sh <target>` runs a single Makefile target in the same
sandbox.

Do not proceed to the next sub-task until the current one is green.

### Diff-sized local check and waiver

Use the affected-check dispatcher for the fast inner loop and for local PR
readiness when a full run would stall progress. It is the `affected-checks`
subcommand of the Rust engine — `td-builder affected-checks` (run from the repo
root). Resolve `td-builder` to a prebuilt binary if you have one
(`$TD_BUILDER`, `builder/target/release/td-builder`, or a `td-builder` on PATH),
else build it once: `cargo build --release --manifest-path builder/Cargo.toml`.

```
td-builder affected-checks        # show changed paths and selected checks
td-builder affected-checks --run  # run the selected preflights and ./check.sh targets
```

It compares the branch to `origin/main` (falling back to `main`) and includes dirty,
staged, and untracked files by default. Use `--committed-only` before push/PR review
when you want the committed branch diff only, or `--path FILE` to inspect the mapping
for a specific file.

After rebasing for PR readiness, run:

```
td-builder affected-checks --committed-only --run
```

If it prints `Waiver: full ./check.sh waived by affected-checks for this diff`,
that is the local waiver for the full loop; record the selected checks and waiver
line in the PR body. If it prints `Waiver: full ./check.sh required before
marking ready`, `--run` executes the selected checks and then the full `./check.sh`
before it can exit successfully. Full-loop escalation is mandatory for changes the
dispatcher cannot classify, changes to the loop spine (`check.sh`, `Makefile`,
`system/td.scm`, `DIGESTS.md`), CI/runner gating, and channel pin changes.

**Build-engine changes (`builder/src/*`) are the exception (human 2026-06-21):** they
no longer escalate to the full loop — they validate on the **`check-engine` smoke tier**
(`./check.sh check-engine`: a TRUE ~2-min smoke — cheap structural gates + `cargo-test`
(compile the engine + its unit tests), and NOTHING that builds a package from source) and
`affected-checks` waives the full loop for them. The full heavy+system suite is no longer
a per-PR gate; it runs **once daily** on fresh main via `ci/daily-full-suite.sh`, driven
by a scheduled agent that opens a **fix-or-revert PR (no auto-merge)** on any regression
(DESIGN §7.2). A corpus/system regression the smoke misses is healed within a day, not
blocked per-PR — the accepted velocity trade.

## Verified-red discipline

A green behavioral gate is only meaningful once you have SEEN it red. For every new
assertion, deliberately break the thing it checks and watch the test fail before
trusting the pass; record the verified-red evidence in your commit message (and the
PR body). (This discipline caught a three-defect false-green that survived M1–M3 —
full story in `HISTORY.md`.)

## Differential + durable discipline

The migration is proven by differentials against Guix (directive 4: build it both
ways, diff the store paths — Guix is the oracle). But a differential is *migration
scaffolding*: the day Guix is retired it cannot run and stops meaning anything. A
gate that asserts ONLY "td == Guix, byte-identical" leaves nothing behind — it would
have to be rewritten, not deleted, when the oracle goes. So:

- **Every reconstruction/replacement gate must carry at least one DURABLE assertion**
  — one that still holds with no Guix oracle in the room. Durable assertions are:
  *behavioral* (the artifact actually does its job — the tool runs, the system
  boots, a round-trip succeeds), *intrinsic-reproducibility* (`td-builder check`'s
  own double-build, not `guix build --check`), *structural self-consistency* (the
  output has the expected shape; td writes a store path and reads it back; the
  closure is complete), and the *self-discrimination* legs (a perturbation diverges;
  the input is load-bearing).
- **The Guix byte-identity / NAR-equality legs are the removable "migration
  oracle."** Label them as such in the gate so that retiring Guix is a *deletion of
  those lines*, not a test rewrite. Keep them (directive 4 / "own, then diverge" —
  they are a guardrail, not the whole point), but never let them be a gate's only merit.
- A gate that is *purely* a Guix differential is a smell to fix, not a finished gate.

This is the test-design corollary of "own, then diverge": once td owns a capability,
the differential guards it; the durable assertions are what we actually keep.

## Definition of done (every task)

A task is done only when ALL hold:

- a test asserts the new behavior and passes,
- you have seen that assertion fail (verified-red) before trusting the pass,
- the build is reproducible (`guix build --check` passes),
- the change is the smallest increment that turns one test green,
- it is committed with a message stating what test now passes,
- it is landed on main via the landing protocol below.

If any are missing, the task is not done.

## Parallel work (worktrees, merge on green)

Multiple agents work this repo concurrently. The unit of work is a **branch + draft
PR**. There is no claim file and no generated status index — *the open-PR list is the
record of who is working on what*, and all working notes live in the git log + PR
body. DESIGN §7.1's roadmap stays as a descriptive list of workstreams, not a claim
ledger.

- **Claim by opening a draft PR.** Before starting, scan the open PRs
  (`gh pr list`) so two agents don't pick the same work; if one is already open for it,
  pick something else. Open your **draft PR** early (main is branch-protected; nothing
  lands directly) — that draft, with a title naming the workstream, IS your claim.
  Release it by closing the PR if you abandon the work.
- **Work in your own git worktree/branch** (`git worktree add ../td-<name>`), never on
  a shared checkout of main. Your running notes, sub-task ladder, and verified-red
  evidence go in your **commit messages and the PR body** — never in a tracked file. Do
  not create files to track or claim work; tracking and claiming are the open PRs, full
  stop.
- **Land (optimistic merge on green, via PR):** main is **non-strict** (DESIGN
  §7.2, human 2026-06-19) — a PR merges on **its own** green checks; main moving
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

## Workflow

1. Claim your work by opening a draft PR (after scanning open PRs to avoid a
   collision); read the relevant DESIGN §7.1 acceptance test.
2. Before writing implementation, state the sub-task and write (or name) the test that
   will verify it.
3. Make the smallest change that turns that test green; verify red first.
4. Run the loop. If red, fix forward — never by weakening the test.
5. Commit small green increments on your branch as you go — no per-commit review is
   required; the PR-level review in step 6 covers the whole branch.
6. **Before the PR goes up for human review, get a code review of the FULL branch
   diff — MANDATORY for AI agents.** Spawn a DEDICATED reviewer subagent (run
   `/code-review`) — an independent context, NOT the implementing agent reviewing
   its own diff; that independence is the point and is why this is one review of the
   branch, not per-commit self-review. Review the whole branch against this
   contract — correctness bugs AND no weakened gates/assertions, smallest increment,
   conventions respected — and address its findings BEFORE opening the PR or marking
   it ready. This single PR-level review is the one required agent review; it
   PRECEDES, never replaces, the human's PR review (DESIGN §4.3).
7. Land per the protocol when the acceptance test is green.

Prefer many small green commits over one large change. If a change spans layers, split
it.

## When stuck or blocked

- If a test cannot pass honestly, STOP and report what is blocking — do not fake,
  stub-to-green, or disable.
- If a build needs something not declared, declare it properly; do not reach outside the
  container.
- Off-roadmap work needs no permission — build it and open a PR; the human's review
  is the approval (DESIGN §4.3). The one thing to never do silently is weaken the loop:
  surface any loosened/removed gate in the PR (directive 3). Everything merges on green
  + PR approval (§7.2).

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
- `tests/` — marionette system tests in the `(gnu tests)` style, plus the
  differential/coverage gates.
- `channels.scm` — pinned Guix channel commit. Reproducibility is anchored here; bump it
  deliberately (exclusive landing), never silently.
- `DESIGN.md` — the settled contract: loop, target, invariants, roadmap (§7.1),
  parallel-work rules (§7.2–7.4).
- Work tracking: there is **no `PLAN.md` and no per-PR tracking/claim files** — claims
  are the open draft PRs (`gh pr list`), and work notes + verified-red evidence live in
  commit messages + the PR body (the squash merge preserves the commit messages in
  `git log`). `M10-design.md` — the M10 design note.
- `td-builder affected-checks` (`builder/src/affected.rs`) — diff-to-check
  dispatcher for local iteration and PR readiness. Run it first to see the selected
  checks, then `td-builder affected-checks --run` to execute them. Its waiver line
  decides whether the local full `./check.sh` run is waived or still required;
  `--run` enforces escalation by running the full loop when required. Its own mapping
  guard is `td-builder affected-checks --self-test` (also a `cargo-test` `#[test]`).
- `HISTORY.md` — completed-milestone record. `DIGESTS.md` — reproducibility record
  (changes only on oracle re-baseline; exclusive landing).

**Naming & formatting**

- Scheme files: lowercase kebab-case (`td.scm`, `boot.scm`). Modules carry a `td`
  prefix.
- Hand-formatted 2-space indentation, no tabs. Do NOT run `guix style` — it was tried
  in M2 and mangled the layout; the hand-formatted style is the convention.
- Run every build/test via `./check.sh` (or `./check.sh <target>`), which enters td's
  own `td-builder host-sandbox` for you (see "The loop"). Don't add `--network` to
  it — that pulls substitutes (offline/hermeticity violation).

**Free-software posture**

- Relaxed to a **non-goal** (human, 2026-06-11 — DESIGN §5). The pinned channel stays
  the default source; nonfree inputs (firmware, blobs, crates, the `nonguix` channel)
  may be adopted when a task needs them, declared and pinned like any other input.
  Unchanged: the loop stays offline with substitutes disabled — that is a
  reproducibility rule, not a free-software rule. Note: a full-source bootstrap of the
  toolchain at `/td/store` **is** now a goal (the North Star, sharpened 2026-06-21 — no
  guix *bytes*, so a guix-captured seed is rejected and the toolchain is built from
  source), but it is pursued for *guix independence*, not FSDG purity; nonfree inputs
  stay adoptable where a task needs them.

**Commits**

- Small green increments. Each commit message states which test now passes (e.g.
  "boot test asserts expected kernel via uname -r"). Prefer many small commits over one
  large change. The full branch is reviewed once by an independent code-review
  subagent before the PR goes up — not per commit (Workflow step 6). Land on
  main only via the §7.2 protocol (rebase → affected-checks waiver or full escalation
  → PR → CI green + human approval → squash-merge).
- **Commit messages ARE the durable record.** main takes only squash merges (merge and
  rebase merges are disabled), and the repo composes the squash commit's body from your
  branch's commit messages, not the PR description (`squash_merge_commit_message =
  COMMIT_MESSAGES`). So put the rationale, the design decisions, and the verified-red
  evidence in your commit messages — that is what lands in `git log` on main. The PR
  body is review context for the human and does NOT persist into git; don't rely on it
  to record anything you want to keep.
