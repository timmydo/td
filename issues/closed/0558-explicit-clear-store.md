---
title: Make clearing the ladder store explicit (drop the implicit per-run wipe)
labels: [infra, recipes]
blocked-by: none
---

## What

`RecipeCheckRunner::setup()` cleared persisted ladder state on every run: in the
per-worktree cold mode it wiped the whole ladder work dir (the force-cold
clean-room proof), and in ALL modes it reset the seed store/db (`<lw>/store`,
`<lw>/db`). In the shared daemon daily this reset silently dropped a completed
toolchain out from under a concurrent run: with `TD_CHECK_BUILD_REUSE` off the
build outputs live in `<lw>/store`, so a neighbor daily grabbing the ladder lock
and running `setup()` reset the store and cold-climbed — the surprise the
operator hit.

Clearing the store is now an EXPLICIT command. `setup()` never destroys
persisted state; the only reset is `td-recipe-eval clear-store`.

## Entry points

- `recipes/src/bin/td_recipe_eval/check_runner.rs` — `setup()` (no implicit
  deletes), the removed `force_cold` field, `ladder_work_dir()` resolver,
  `clear_store_cli`/`clear_ladder`, `explicit_ladder_cache_cap()` (eviction
  opt-in), and the `with_seed_reset_hint` actionable error.
- `recipes/src/bin/td-recipe-eval.rs` — the `clear-store` subcommand + usage.
- `builder/src/check_loop.rs`, `recipes/src/bin/td_recipe_eval/checks/run.rs` —
  stale force-cold rationale comments updated to the explicit `clear-store`.
- `builder/src/daily.rs` — the heavy leg pins `TD_CHECK_BUILD_REUSE=""` so the
  daily's from-stage0 proof survives the removed wipe.
- `builder/src/main.rs` — `authenticate_seed_db` prefixes a torn-db parse red
  with `plan seed db` so the runner's `clear-store` hint fires on it.

## Done

- `setup()` retains the seed store/db and the shared build-cache; a stale/torn
  seed reds with a `clear-store` hint instead of self-healing.
- `td-recipe-eval clear-store` removes the whole ladder work dir under its lock,
  leaving the sibling `<lw>.lock` intact.
- Over-cap build-cache eviction is opt-in (`TD_CHECK_LADDER_CACHE_CAP_BYTES`
  set); unset ⇒ no implicit reclaim.
- The daily heavy leg's from-stage0 toolchain proof is PRESERVED. Build outputs
  live in the per-invocation scratch (`<lw>/scratch/<pid>/tdstore`), which
  `setup()` still recreates fresh each run, and the heavy leg now pins
  `TD_CHECK_BUILD_REUSE=""` so `build-plan` cold-climbs the whole chain from
  stage0 into its own in-run store — the explicit pin replaces what the removed
  wipe used to guarantee implicitly (an ambient `TD_CHECK_BUILD_REUSE=1` could
  otherwise warm-reuse the shared build-cache). Only the pin-verified seed store
  is now retained instead of wiped; `clear-store` remains available for an
  operator who wants a bare-tree reset (not auto-wired into the daily — accepted).
- Consequence of retaining the seed store/db: a pinned-seed change (or a torn
  intern) now reds `authenticate_seed_db` on the retained ladder — including
  per-change Shared gates on that host — until an explicit `clear-store`, rather
  than silently self-healing via the old per-run db reset. The red carries a
  `clear-store` hint naming the exact ladder (fail-closed, never silently wrong);
  a torn/truncated seed db is prefixed `plan seed db` so it, too, gets the hint.

## Collisions

Touches `recipes/src/bin/td_recipe_eval/check_runner.rs`,
`recipes/src/bin/td-recipe-eval.rs`, a small `authenticate_seed_db` error-prefix
edit in `builder/src/main.rs`, the daily heavy-leg env list + comment in
`builder/src/daily.rs`, and comment-only edits in `builder/src/check_loop.rs`
and `recipes/src/bin/td_recipe_eval/checks/run.rs` — coordinate with anything
else editing the ladder runner or the check loop's env-forwarding block. The
`daily.rs` change adds one env tuple to the heavy leg + refreshes its comment;
`issue-0555-daily-*` also lives in `daily.rs`, so expect a small merge there at
landing.
