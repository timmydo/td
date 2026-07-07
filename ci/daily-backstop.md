# Daily full-suite backstop — operations

The full `td-builder check` (heavy + daily + system) is **not a per-PR blocking gate**
(DESIGN §7.2; per-PR budget ~10 min, human 2026-07-04). Engine PRs validate on
`check-engine` and everything else on `check-pr`; the `daily` gate pool (the deep
bootstrap rungs + the from-source corpus) runs ONLY here. The whole suite runs **once
daily** on fresh main, and a scheduled agent heals any regression with a **fix-or-revert
PR (no auto-merge — a human merges)**. This note is how that agent is run.

## The two halves

- **Mechanical runner — `td-builder daily`** (`builder/src/daily.rs`): fetches fresh
  main, runs `td-builder check` (heavy+daily) + `check-system`, and ALWAYS also attempts
  the `check-harness` leg (the /td/store harness tier) — its precondition is "harness
  locally persisted or fetchable from a substitute store", independent of whether
  heavy/system ran. Writes `.td-daily-verdict` (machine-readable) and, on all-green,
  records `.td-last-green`.
  - **Exit is a bitfield** over REAL regressions: 1 heavy red, 2 system red, 4 harness
    red. A leg the runner isn't provisioned for does not set its bit — its
    `td-builder check` exits `69` (`EXIT_UNPROVISIONED`, a stable signal, not FATAL
    prose), recorded as `env_error` (heavy/system: loop toolchain unresolved) or
    `harness_env_error` (no local/fetchable harness) in the verdict.
  - **Exit 10**: unprovisioned for EVERY leg — nothing ran anywhere, nothing to triage.
  - **Exit 8/9**: bad CLI arg / `git fetch origin main` failed (or no td-builder to run).
  - A runner unprovisioned for heavy/system but with a reachable harness does NOT hit
    exit 10: heavy/system are marked `unprovisioned` (not regressions) while the harness
    leg's own green/red reaches the verdict — printed as `PARTIAL` (harness green, nothing
    to revert, `.td-last-green` NOT recorded) or a real `rc` bit (harness red).
- **The agent** — judgment: runs the runner, and on a real (non-unprovisioned) red
  triages the suspect range and opens a fix-or-revert PR. Run it daily as a fresh
  headless agent (NOT inside a working session).

## Agent prompt (what the daily agent does)

```
You are the td daily full-suite backstop. The repo is at <REPO>. Steps:
1. cd <REPO>; build the engine: cargo build --release --manifest-path builder/Cargo.toml
2. Run: builder/target/release/td-builder daily --verdict .td-daily-verdict
3. Read .td-daily-verdict.
   - Exit 10: unprovisioned for every leg — a HOST setup gap, not a code regression.
     Report it and stop; no PR.
   - env_error=1 with harness green (PARTIAL): heavy/system didn't run (loop toolchain
     unresolved) but nothing that DID run is red — report it and stop; no PR.
   - Any leg genuinely red (heavy_rc/system_rc/harness_rc nonzero and NOT explained by
     env_error / harness_env_error): a real regression — proceed to step 4.
   - Otherwise (all green): report green, stop.
4. On a real red: the suspect is in `git log $(cat .td-last-green 2>/dev/null || echo origin/main~10)..origin/main`.
   Reproduce the failing gate (the *_fail field names it) to confirm it is a real
   regression, not host contention (re-run that one gate in isolation). If real, open a
   FIX-OR-REVERT PR with `ci/revert-suspect.sh --ref <sha> --open-pr` (revert) or a small
   forward fix — DO NOT auto-merge; leave it for human review. Post the verdict + the
   suspect + the PR link.
Use the timmydo-bot gh credentials (~/.local/bin/gh). Never merge; a human merges.
```

## Enabling the daily schedule (host cron — a host change, enable deliberately)

A daily headless agent, e.g. at 04:17 local (off the :00 mark so the fleet doesn't synchronize):

```cron
17 4 * * *  cd <REPO> && claude -p "$(sed -n '/^You are the td daily/,/a human merges\./p' ci/daily-backstop.md)" >> ~/.td-daily-backstop.log 2>&1
```

`claude -p` runs one headless agent per fire — genuinely "another agent, daily",
independent of any working session. (A `systemd --user` timer is equivalent and survives
logout.) This touches the dev host's crontab, so it is enabled by the operator, not
auto-installed.

## Session-local alternative (not recommended for permanence)

A `CronCreate` job fires the prompt into the *current* Claude session and auto-expires
after 7 days — fine as a one-week live starter, but it ties up the working session for the
~30-min run and is not a durable separate agent. Prefer the host cron above.
