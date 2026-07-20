# Daily full-suite backstop — operations

The full `td-builder check` (heavy + daily + system) is **not a per-PR blocking gate**
(DESIGN §7.2; per-PR budget ~10 min, human 2026-07-04). Engine changes validate on
`check-engine` and everything else on `check-pr`; the `daily` gate pool (the deep
bootstrap rungs + the from-source corpus) runs ONLY here. The whole suite runs **once
daily** on fresh main, and a scheduled agent heals any regression with a **fix-or-revert
that the integrator lands (no auto-land — a human integrates)**. This note is how that
agent is run. GitHub (and the sr.ht mirror) is a git backup remote only; nothing here
touches the GitHub API.

## The two halves

- **Mechanical runner — `td-builder daily`** (`builder/src/daily.rs`): fetches fresh
  main, runs `td-builder check` (heavy+daily) + `check-system`. Writes
  `.td-daily-verdict` (machine-readable) and, on all-green,
  records `.td-last-green`.
  - **Exit is a bitfield** over REAL regressions: 1 heavy red, 2 system red.
    A leg the runner isn't provisioned for does not set its bit — its
    `td-builder check` exits `69` (`EXIT_UNPROVISIONED`, a stable signal, not FATAL
    prose), recorded as `env_error` (heavy/system: loop toolchain unresolved) in the verdict.
  - **Exit 10**: unprovisioned for EVERY leg — nothing ran anywhere, nothing to triage.
  - **Exit 8/9**: bad CLI arg / `git fetch origin main` failed (or no td-builder to run).
- **The agent** — judgment: runs the runner, and on a real (non-unprovisioned) red
  triages the suspect range, forms a fix-or-revert, and records the regression as a
  backlog file. Run it daily as a fresh headless agent (NOT inside a working session).

## The daily-red backlog signal

On a real regression the agent writes (or refreshes) `issues/open/daily-red.md` —
a single rolling item carrying the verdict, the suspect range, and the triage
instructions — so `ls issues/open/` (the backlog menu, AGENTS.md "Parallel work")
surfaces it. When the fix/revert lands, `git mv issues/open/daily-red.md
issues/closed/` in the landing commit. Keep it to one open daily-red file; append to
it rather than creating a second.

## Agent prompt (what the daily agent does)

```
You are the td daily full-suite backstop. The repo is at <REPO>. Steps:
1. cd <REPO>; build the engine: cargo build --release --manifest-path builder/Cargo.toml
2. Run: builder/target/release/td-builder daily --verdict .td-daily-verdict
3. Read .td-daily-verdict.
   - Exit 10: no current leg ran — a HOST setup gap, not a code regression.
     Report it and stop; no backlog file.
   - env_error=1: heavy/system didn't run (loop toolchain unresolved) — report it
     and stop; no backlog file.
   - Any current leg genuinely red (heavy_rc/system_rc nonzero and NOT explained by
     env_error): a real regression — proceed to step 4. The retired harness field is
     compatibility output, not a current regression leg.
   - Otherwise (all green): report green; if issues/open/daily-red.md exists and main
     is green now, git mv it to issues/closed/. Stop.
4. On a real red: the suspect is in `git log $(cat .td-last-green 2>/dev/null || echo origin/main~10)..origin/main`.
   Reproduce the failing gate (the *_fail field names it) to confirm it is a real
   regression, not host contention (re-run that one gate in isolation). If real, form a
   fix-or-revert: a small forward fix, or a revert with `ci/revert-suspect.sh --ref <sha>`
   (which creates heal/revert-<sha>). Write the verdict + suspect + triage steps into
   issues/open/daily-red.md. DO NOT land it — leave the fix/revert branch for the
   integrator (a human integrates; AGENTS.md "Parallel work"). Report the verdict + the
   suspect + the branch name.
```

`claude -p` runs one headless agent per fire — genuinely "another agent, daily",
independent of any working session.

## Enabling the daily schedule (host cron — a host change, enable deliberately)

A daily headless agent, e.g. at 04:17 local (off the :00 mark so the fleet doesn't synchronize):

```cron
17 4 * * *  cd <REPO> && claude -p "$(sed -n '/^You are the td daily/,/the branch name\./p' ci/daily-backstop.md)" >> ~/.td-daily-backstop.log 2>&1
```

This touches the dev host's crontab, so it is enabled by the operator, not
auto-installed. (A `systemd --user` timer is equivalent and survives logout.) The
runner needs the warm loop prelude + a resolvable loop toolchain for an authoritative
from-seed daily; on an unprovisioned box it exits `10`/`69` (a host gap, not a
regression) and writes no backlog file.

## Session-local alternative (not recommended for permanence)

A `CronCreate` job fires the prompt into the *current* Claude session and auto-expires
after 7 days — fine as a one-week live starter, but it ties up the working session for the
~30-min run and is not a durable separate agent. Prefer the host cron above.
