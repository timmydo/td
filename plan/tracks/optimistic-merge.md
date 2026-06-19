section: side
status: done
title: optimistic-merge
handle: claude-opus-afc8a2
date: 2026-06-19
pr: 103
notes: plan/optimistic-merge.md
summary: drop the merge-time green(A∪B) guarantee for velocity — flip the ruleset off `strict_required_status_checks_policy` so PRs no longer force rebase-onto-tip + full re-run, rewrite the §7.2 landing protocol to "green against your base → merge; rebase only on real git conflict", and add the safety net as an AGENT DUTY (human pivot 2026-06-19, no automated workflow / no HEAL_PAT): on a red `check-fast` on main, an agent runs `ci/revert-suspect.sh --open-pr` to revert the suspect squash commit with its own bot creds. Heavy-only breaks wait for the next manual full run (documented, accepted gap).
