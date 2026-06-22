# Daily full-suite backstop — operations

The full `./check.sh` (heavy + system) is **no longer a per-PR blocking gate** (DESIGN §7.2,
human 2026-06-21). Engine PRs validate on the `check-engine` smoke tier; the whole suite runs
**once daily** on fresh main, and a scheduled agent heals any regression by opening a
**fix-or-revert PR (no auto-merge — a human merges)**. This note is how that agent is run.

## The two halves

- **Mechanical runner — `ci/daily-full-suite.sh`** (in repo): fetches fresh main, runs
  `./check.sh` + `./check.sh check-system`, writes `.td-daily-verdict` (machine-readable) and,
  on all-green, records `.td-last-green` (the seed of a future "stable" marker). Exit: 0 green,
  1 heavy red, 2 system red, 3 both.
- **The agent** — judgment: runs the runner, and on red triages the suspect range and opens a
  fix-or-revert PR. Run it daily as a fresh headless agent (NOT inside a working session).

## Agent prompt (what the daily agent does)

```
You are the td daily full-suite backstop. The repo is at <REPO>. Steps:
1. cd <REPO>; run: ci/daily-full-suite.sh --verdict .td-daily-verdict
2. Read .td-daily-verdict. If heavy=green and system in {green,skipped}: report green, stop.
3. On red: the suspect is in `git log $(cat .td-last-green 2>/dev/null || echo origin/main~10)..origin/main`.
   Reproduce the failing gate (the *_fail field names it) to confirm it is a real regression,
   not host contention (re-run that one gate in isolation). If real, identify the squash commit
   that introduced it and open a FIX-OR-REVERT PR with `ci/revert-suspect.sh --ref <sha> --open-pr`
   (revert) or a small forward fix — DO NOT auto-merge; leave it for human review. Post the
   verdict + the suspect + the PR link.
Use the timmydo-bot gh credentials (~/.local/bin/gh). Never merge; a human merges.
```

## Enabling the daily schedule (host cron — a host change, enable deliberately)

A daily headless agent, e.g. at 04:17 local (off the :00 mark so the fleet doesn't synchronize):

```cron
17 4 * * *  cd <REPO> && claude -p "$(sed -n '/^You are the td daily/,/a human merges\./p' ci/daily-backstop.md)" >> ~/.td-daily-backstop.log 2>&1
```

`claude -p` runs one headless agent per fire — genuinely "another agent, daily", independent of
any working session. (A `systemd --user` timer is equivalent and survives logout.) This touches
the dev host's crontab, so it is enabled by the operator, not auto-installed.

## Session-local alternative (not recommended for permanence)

A `CronCreate` job fires the prompt into the *current* Claude session and auto-expires after 7
days — fine as a one-week live starter, but it ties up the working session for the ~30-min run
and is not a durable separate agent. Prefer the host cron above.
