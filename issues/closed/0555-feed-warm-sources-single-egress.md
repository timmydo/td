---
title: td-feed warm sources single-egress must not depend on TD_FEED_BASE
labels: [feed, bootstrap-prep]
blocked-by: none
---

## What

`td-feed warm sources` egresses each pinned source tarball ONCE across all
worktrees only when the caller exports `TD_FEED_BASE` (the /loop prelude does,
after `td-feed ensure-serve`). Any invocation without that env — ad-hoc,
background, a bare `td-feed warm sources` — silently drops to a per-worktree
direct GET, so ~8 worktrees each re-fetch the same gcc/binutils/glibc tarball,
and one wedged mirror read (a stalled ftp.gnu.org HTTPS connection with no read
timeout) blocks that worktree's whole build. Make single-egress self-sufficient:
warm sources should bring the shared feed daemon up itself when `TD_FEED_BASE`
is unset.

## Entry points

- `feed/src/main.rs`: `warm_sources()`, `shared_feed()`, `ensure_serve()`.
- `builder/src/check_loop.rs`: `heavy_warms()` (the prelude that runs
  `ensure-serve` then exports `TD_FEED_BASE`).

## Done

`shared_feed()` returns a usable `(addr, store)` even with `TD_FEED_BASE` unset
by ensuring/discovering the shared daemon on the default store
(`$TD_FEED_DIR` else `$HOME/.td/feed`) via the same lock-serialized,
reuse-a-live-daemon lifecycle as `ensure-serve`; `warm_one` then egresses only
when the shared store is cold. A hermetic unit test asserts the reuse path
returns a live daemon's recorded address without spawning, and an end-to-end run
shows a second caller reuse the first caller's daemon (same addr) rather than
direct-fetching.

## Collisions

Touches `feed/src/main.rs` only. No exclusive-landing files
(`builder/src/gates.rs`, `builder/src/check_loop.rs`) or regenerated baselines.
Disjoint from `0554-migrate-off-github`.
