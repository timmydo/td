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
   sign-off first — this is the one permanent human gate (DESIGN §4.3), no matter the
   justification. Adding or strengthening tests is always free. If a test cannot pass
   honestly, STOP and report.
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
underlying target (assumes you are already in that sandbox); it runs, in order and
short-circuiting on the first failure: config eval → the differential rungs →
`guix build --check` on the targets → the marionette system tests → the manifest and
generation rungs. It exits non-zero on any failure.

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
  (a tiny standalone commit straight to main, pushed). One agent per track; if a line
  is claimed and fresh, pick another track. Release the claim when you land or stop.
- **Work in your own git worktree/branch** (`git worktree add ../td-<track>`), never
  on a shared checkout of main. Keep running notes, sub-task ladder, and verified-red
  evidence in `plan/<track>.md` — never edit another track's file.
- **Land (merge on green):** (1) fetch + rebase onto latest `origin/main`; (2) run
  the FULL `./check.sh` — must be green; (3) fast-forward main and push; (4) if main
  moved while checking, repeat from (1). No PRs, no human merge. Landing without a
  green full check is a contract violation.
- **Exclusive landings:** changes to the shared spine — `system/td.scm` (frozen
  oracle), `check.sh`, `Makefile`, `channels.scm`, `DIGESTS.md` — collide with
  everyone. Announce in your track file, land as small standalone commits, expect
  others to rebase. Oracle re-baselines and channel bumps are the canonical cases.
- **Resources:** each full check boots QEMU VMs; two concurrent checks are fine, more
  may thrash. Stagger if the host is loaded.

## Workflow

1. Claim a track; read its `plan/<track>.md` and its DESIGN §7.1 acceptance test.
2. Before writing implementation, state the sub-task and write (or name) the test that
   will verify it.
3. Make the smallest change that turns that test green; verify red first.
4. Run the loop. If red, fix forward — never by weakening the test.
5. Commit a small increment on your branch. Land per the protocol when the track's
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
  DESIGN §4.3. Everything on the roadmap merges on green.)

## Repo conventions

**Directory layout**

- `check.sh` — the canonical hermetic, offline pass/fail command (`./check.sh`). The
  only command you need to determine green/red.
- `Makefile` — the `make check` target it runs inside that sandbox (config eval →
  differentials → `guix build --check` → marionette tests → manifest/generation rungs).
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
  it — that pulls substitutes (FSDG + offline violation).

**Free-software posture**

- Strict FSDG (Guix's free-software guidelines). No nonfree firmware, blobs, or crates.
  Do not add the `nonguix` channel. If a task appears to require nonfree code, STOP and
  ask.

**Commits**

- Small green increments. Each commit message states which test now passes (e.g.
  "boot test asserts expected kernel via uname -r"). Prefer many small commits over one
  large change. Land on main only via the §7.2 protocol (rebase → full green check →
  fast-forward push).
