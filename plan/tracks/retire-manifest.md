section: side
status: done
title: retire-manifest
handle: claude-opus-3267ea
date: 2026-06-20
pr: 113
notes: plan/retire-manifest.md
summary: retire tests/td-chained-edges.txt — the build-plan gate (365) derives its subjects from the recipe graph and builds each via `build-plan --auto`; the guix-dependence census derives edge-owned from the graph too. Edge-ownership infra self-maintains: a new recipe's edges chain + get credited automatically. Stacks on #110 (--auto).
