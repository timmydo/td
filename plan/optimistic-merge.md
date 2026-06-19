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

1. [ ] Ruleset: `strict_required_status_checks_policy` → false in
   `.github/setup-branch-protection.sh` (+ comment). Keep lint+check-fast
   required, linear history, squash. (Human applies; bot lacks admin.)
2. [ ] Contract rewrite: DESIGN §7.2 + CLAUDE.md "Parallel work" landing
   protocol → "green against your base → merge; rebase only on real git
   conflict"; document the optimistic-merge + auto-revert-heal model and the
   accepted heavy-only-break gap.
3. [ ] Heal primitive: `ci/revert-suspect.sh` — given main's HEAD, form the
   revert (locally testable kernel; verify-red its suspect-selection).
4. [ ] Heal workflow: `.github/workflows/heal-main.yml` — on `check-fast` red on
   main, run the script to open an auto-revert PR.
5. [ ] Gate: a cheap structural gate asserting the heal script picks the right
   suspect (verified-red).

## Human-side actions (call out in PR — can't be done by the bot)

- Apply the updated ruleset (`./.github/setup-branch-protection.sh`, admin auth).
- Provision the heal workflow's token: `GITHUB_TOKEN`-opened PRs do NOT trigger
  checks (recursion guard), so the revert PR needs a bot PAT secret to get its
  check-fast run + auto-merge. Decide whether revert PRs may merge without a
  human review (true zero-latency heal) or wait for approval.

## Verified-red evidence

(to fill in)
