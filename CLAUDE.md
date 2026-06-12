# CLAUDE.md — td

You are one of possibly several agents building a functional Linux distribution
incrementally on top of an existing Guix system. You grow the OS *inside* a
verification loop: you do not get credit for code, only for a passing, reproducible
test. Read `DESIGN.md` for the target, the approved roadmap (§7.1), and the
parallel-work rules (§7.2–7.4).

This file is your contract. The rules below are absolute and override any local
convenience.

## Prime directives (never violate)

1. **Reproducibility is a test.** Every artifact must pass `guix build --check`. A
   non-reproducible build is a FAILING test. Never commit a non-reproducible artifact.
2. **Hermeticity.** No undeclared dependencies. Builds run offline except declared
   fixed-output fetches. Never make a build pass by reaching outside the container,
   adding an undeclared dependency, or disabling `--check`.
3. **Never weaken a test to pass it.** Do not skip, delete, comment out, loosen, or
   `xfail` a test to turn a task green. Removing or loosening ANY existing rung or
   assertion in `check.sh`, the `Makefile`, or `tests/` requires explicit human
   sign-off first — one of the two permanent human gates (DESIGN §4.3; the other is
   roadmap additions), no matter the justification. Adding or strengthening tests
   is always free. If a test cannot pass honestly, STOP and report.
4. **Differential testing before replacement.** Never replace a Guix component
   (`guix-daemon`, store, config language, etc.) without first proving behavioral
   equivalence to the original — build the same thing both ways and diff the store
   paths. The existing component is the oracle.
5. **Stay on the roadmap.** Work only on tracks listed in DESIGN.md §7.1 — that list
   is binding scope, pre-approved by the human, so within it you implement without
   waiting for sign-off. Anything not on it is out of scope: STOP and ask rather than
   expand scope on your own.
6. **Respect the state boundary.** The VM is ephemeral per test (fresh state, wiped on
   reset) — that is *test isolation*, not a ban on persistence *within* a test:
   a guest that reboots mid-test and keeps its placed generations (M10) is legitimate
   behavior under test. `/gnu/store` and the system declaration are immutable. What
   may persist on a machine is default-deny and declared (DESIGN §2.6): only
   allowlisted paths on the `td-state` filesystem survive a generation swap; machine
   identity (SSH host keys) lives there, never in a generation's root. Never stash
   mutable state outside the declared boundary to make something work.

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

`./check.sh` is the canonical hermetic entry point: it sets up the fresh
`guix shell -C --pure` container (store/cache/daemon-socket exposure, host guix on
PATH, **substitutes disabled** so the loop is offline), guards that the host guix
matches the `channels.scm` pin, then runs `make check` inside it. `make check` is the
underlying target (assumes you are already in that sandbox); it runs the rung ladder
(structural rungs serial-first, heavy rungs two at a time), short-circuiting on the
first failure, and exits non-zero on any failure.
The `Makefile`'s `CHEAP_RUNGS`/`HEAVY_RUNGS` pools (expanded by its `check:` target)
are the **authoritative rung list** — the one place it is written down; never restate
it in docs. Broad shape: config eval →
differentials → `guix build --check` → behavioral/marionette tests.

Every build/test runs inside that fresh container (`guix shell -C --pure`) so your own
environment cannot contaminate results; `./check.sh <target>` runs a single Makefile
target in the same sandbox.

Do not proceed to the next sub-task until the current one is green.

## Verified-red discipline

A green behavioral rung is only meaningful once you have SEEN it red. For every new
assertion, deliberately break the thing it checks and watch the test fail before
trusting the pass; record the verified-red evidence in your track file. (This
discipline caught a three-defect false-green that survived M1–M3 — full story in
`HISTORY.md`.)

## Definition of done (every task)

A task is done only when ALL hold:

- a test asserts the new behavior and passes,
- you have seen that assertion fail (verified-red) before trusting the pass,
- the build is reproducible (`guix build --check` passes),
- the change is the smallest increment that turns one test green,
- it is committed with a message stating what test now passes,
- it is landed on main via the landing protocol below.

If any are missing, the task is not done.

## Parallel work (tracks, worktrees, merge on green)

Multiple agents work this repo concurrently. The unit of work is a **track**
(DESIGN §7.1 lists them; `PLAN.md` is the status index).

- **Claim** exactly one track: put your handle + date on its status line in `PLAN.md`
  as the FIRST commit on your track branch, then open a **draft PR** for the branch
  (main is branch-protected; nothing lands directly). Handles must be
  **session-unique** — a model name alone collides when two instances run. Generate
  one at session start and reuse it for all claims and notes:
  `echo "claude-fable-$(od -An -N3 -tx1 /dev/urandom | tr -d ' ')"` (e.g.
  `claude-fable-9af31c`). One agent per track; if a line is claimed and fresh, pick
  another track. Release the claim when you land or stop (close the PR if
  abandoning). Claim status = `PLAN.md` on main plus the open PRs' claim edits;
  track files do not carry it.
- **Work in your own git worktree/branch** (`git worktree add ../td-<track>`), never
  on a shared checkout of main. Keep running notes, sub-task ladder, and verified-red
  evidence in `plan/<track>.md` — never edit another track's file.
- **Land (merge on green, via PR):** (1) fetch + rebase onto latest `origin/main`;
  (2) run the FULL `./check.sh` — must be green; (3) push the branch and mark the PR
  ready — CI re-runs the gate and a human review approves (main is branch-protected:
  required checks + mandatory review, no direct pushes — `.github/BRANCH-PROTECTION.md`);
  (4) merge once green and approved — default to arming auto-merge when you mark
  the PR ready (`gh pr merge --auto --squash`, or `--rebase`) so the human's
  approval is the last manual step; merge manually instead when the landing must
  be sequenced (e.g. exclusive landings stacked behind another PR). If main moved
  before the merge, repeat from (1) — auto-merge does not waive the rebase + full
  re-check obligation. Opening a PR with a locally-red or un-run `./check.sh` is a
  contract violation — CI verifies your run, it does not replace it. The
  self-hosted runner's `./check.sh` check becomes required once the `ci-gate`
  track's runner is live (DESIGN §7.1); until then `lint` is the required check
  and step 2 is the only full-loop gate.
- **Exclusive landings:** changes to the shared spine — `system/td.scm` (frozen
  oracle), `check.sh`, `Makefile`, `channels.scm`, `DIGESTS.md` — collide with
  everyone. Announce in your track file, land as small standalone PRs, expect
  others to rebase. Oracle re-baselines and channel bumps are the canonical cases.
- **Resources:** each full check already runs its heavy rungs two at a time (`-j2`),
  so two concurrent checks mean up to four VMs/builds — the observed ceiling. Don't
  add a third check or raise `-j`; stagger if the host is loaded.

## Workflow

1. Claim a track; read its `plan/<track>.md` and its DESIGN §7.1 acceptance test.
2. Before writing implementation, state the sub-task and write (or name) the test that
   will verify it.
3. Make the smallest change that turns that test green; verify red first.
4. Run the loop. If red, fix forward — never by weakening the test.
5. Before each commit, spawn a sub-agent to review the diff against this contract —
   no weakened rungs or assertions, smallest increment, conventions respected — and
   address its findings first.
6. Commit a small increment on your branch. Land per the protocol when the track's
   acceptance test is green. Update `PLAN.md`'s status line as part of landing.

Prefer many small green commits over one large change. If a change spans layers, split
it.

## When stuck or blocked

- If a test cannot pass honestly, STOP and report what is blocking — do not fake,
  stub-to-green, or disable.
- If a build needs something not declared, declare it properly; do not reach outside the
  container.
- If a task seems to require off-roadmap work, STOP and ask — do not expand scope on
  your own. (The two human gates: roadmap additions, and any weakening of the loop —
  DESIGN §4.3. Everything on the roadmap merges on green + PR approval — §7.2.)

## Repo conventions

**Directory layout**

- `check.sh` — the canonical hermetic, offline pass/fail command (`./check.sh`). The
  only command you need to determine green/red.
- `Makefile` — the `make check` target it runs inside that sandbox; its `CHEAP_RUNGS`/
  `HEAVY_RUNGS` pools (expanded by `check:`) are the authoritative rung list (the one
  place it is written down).
- `system/` — Guile system declarations. The frozen oracle lives at `system/td.scm`.
- `tests/` — marionette system tests in the `(gnu tests)` style, plus the
  differential/coverage rungs.
- `channels.scm` — pinned Guix channel commit. Reproducibility is anchored here; bump it
  deliberately (exclusive landing), never silently.
- `DESIGN.md` — the settled contract: loop, target, invariants, roadmap (§7.1),
  parallel-work rules (§7.2–7.4).
- `PLAN.md` — track status index (one line per track). `plan/<track>.md` — per-track
  working state, single writer. `M10-design.md` — the M10 design note.
- `HISTORY.md` — completed-milestone record. `DIGESTS.md` — reproducibility record
  (changes only on oracle re-baseline; exclusive landing).

**Naming & formatting**

- Scheme files: lowercase kebab-case (`td.scm`, `boot.scm`). Modules carry a `td`
  prefix.
- Hand-formatted 2-space indentation, no tabs. Do NOT run `guix style` — it was tried
  in M2 and mangled the layout; the hand-formatted style is the convention.
- Run every build/test via `./check.sh` (or `./check.sh <target>`), which enters the
  `guix shell -C --pure` sandbox for you (see "The loop"). Don't add `--network` to
  it — that pulls substitutes (offline/hermeticity violation).

**Free-software posture**

- Relaxed to a **non-goal** (human, 2026-06-11 — DESIGN §5). The pinned channel stays
  the default source; nonfree inputs (firmware, blobs, crates, the `nonguix` channel)
  may be adopted when a task needs them, declared and pinned like any other input.
  Unchanged: the loop stays offline with substitutes disabled — that is a
  reproducibility rule, not a free-software rule. Mes-style full-source bootstraps
  are likewise a non-goal (DESIGN §5 "Package collection").

**Commits**

- Small green increments. Each commit message states which test now passes (e.g.
  "boot test asserts expected kernel via uname -r"). Prefer many small commits over one
  large change. Every commit is sub-agent-reviewed first (Workflow step 5). Land on
  main only via the §7.2 protocol (rebase → full green check → PR → CI green +
  human approval → rebase/squash-merge).
