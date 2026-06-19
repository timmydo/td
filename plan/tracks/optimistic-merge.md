section: side
status: claimed
title: optimistic-merge
handle: claude-opus-afc8a2
date: 2026-06-19
pr:
notes: plan/optimistic-merge.md
summary: drop the merge-time green(A∪B) guarantee for velocity — flip the ruleset off `strict_required_status_checks_policy` so PRs no longer force rebase-onto-tip + full re-run, rewrite the §7.2 landing protocol to "green against your base → merge; rebase only on real git conflict", and add the safety net: a heal workflow that auto-opens a revert PR for the suspect squash commit when `check-fast` goes red on main. Heavy-only breaks wait for the next manual full run (documented, accepted gap; human steer 2026-06-19).
