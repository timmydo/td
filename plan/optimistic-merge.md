# optimistic-merge — working notes

Handle: claude-opus-afc8a2 · started 2026-06-19

## Goal (human steer 2026-06-19)

Improve PR velocity by dropping the merge-time `green(A∪B)` guarantee. The cost
of the strict landing protocol is: every time main moves, the PR must rebase
onto the new tip and re-run the loop before it can merge. The human's call:
stop requiring that, and instead catch the rare broken combination *after* the
merge and heal it.

Squash is already the merge strategy (ruleset: merge commits off, squash+rebase
on; `gh pr merge --auto --squash` is the default) — so this is NOT a merge-strategy
change. It is a change to the *up-to-date* requirement plus a heal net. Squash is
what makes the heal clean: one commit per merge → an unambiguous revert.

## Decisions

- Heal-on-red policy: **auto-revert the suspect** (human, 2026-06-19).
- Heal locus: **CI on-red only** for now (human, 2026-06-19). The full
  `./check.sh` cannot run per-merge in CI (cold hosted runners can't rebuild
  td's 41G closure; the ci-image is keyed by channel pin, not main commit), so
  the heavy net (dev-box periodic full-loop heal) is deferred. Heavy-only breaks
  (boot/VM/repro) wait for the next manual full run — an accepted, documented gap.

## Plan / sub-tasks

1. [x] Ruleset: `strict_required_status_checks_policy` → false in
   `.github/setup-branch-protection.sh` (+ comment). Keep lint+check-fast
   required, linear history, squash. (Human applies; bot lacks admin.)
2. [x] Contract rewrite: DESIGN §7.2 + CLAUDE.md "Parallel work" landing
   protocol → "green against your base → merge; rebase only on real git
   conflict"; document the optimistic-merge + auto-revert-heal model and the
   accepted heavy-only-break gap.
3. [x] Heal primitive: `ci/revert-suspect.sh` — reverts the suspect on a
   `heal/revert-<sha>` branch (based on current main), loop-guard refuses to
   revert a revert (exit 3), `--open-pr` pushes + opens an auto-merge PR.
4. [x] Heal workflow: `.github/workflows/heal-main.yml` — on the `ci` run
   (check-fast) failing on a push to main, runs the script with `--open-pr`.
5. [x] Behavioral test: `tests/heal-revert.sh`. NOTE: git is NOT in the loop
   sandbox (like diffutils/awk), so this can't be a `./check.sh` gate — wired
   into CI's `lint` job (hosted, has git) in `ci.yml` instead.

## Finding: no git in the loop sandbox

`./check.sh heal-revert` failed with `git: command not found` — the td-builder
host-sandbox toolchain has no git (same class as the known no-diffutils/no-awk
gotcha). A git-driven test therefore cannot be a hermetic loop gate; the heal
primitive's behavioral test runs in CI `lint` (GitHub-hosted) where git exists.

## Human-side actions (call out in PR — can't be done by the bot)

- Apply the updated ruleset (`./.github/setup-branch-protection.sh`, admin auth).
- Provision the heal workflow's token: `GITHUB_TOKEN`-opened PRs do NOT trigger
  checks (recursion guard), so the revert PR needs a bot PAT secret to get its
  check-fast run + auto-merge. Decide whether revert PRs may merge without a
  human review (true zero-latency heal) or wait for approval.

## Full ./check.sh runs

- Run 1 (TD_BUILD_JOBS=4): RED on the `no-guix` gate — `Could not find build
  log for …docker-image.tar.gz.drv`, build failed. Host at the time: 62Gi RAM,
  54Gi used, **0 swap**, 5.3Gi free → the documented no-swap OOM kill (see
  td-full-check-oom memory). The gate is untouched by this diff (docs/CI/shell
  only), so this is environmental, not a real red. Re-running under lower
  memory pressure / parallelism.
- Run 2 (TD_BUILD_JOBS=3, after killing run 1 + waiting out memory pressure):
  **GREEN — EXIT=0**, 39 gate PASSes, no Error/FAIL/OOM, through the final
  rust-russh gate. This is the affected-checks full-loop escalation, satisfied.

## Verified-red evidence

`tests/heal-revert.sh`, both legs (2026-06-19, locally — git present):
- Leg A (revert reverses the suspect): neutered the `git revert` line to an
  empty commit → `FAIL: revert did not restore good content (got 'BROKEN')`,
  exit 1. Restored → PASS.
- Leg B (loop guard): neutered the `exit 3` guard → `FAIL: loop guard did not
  fire on a revert commit (rc=0, want 3)`, exit 1. Restored → PASS.
Both restored to green after.
