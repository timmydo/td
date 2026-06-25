# td-feed-crate-free — working notes

Handle: claude-opus-4b83d3 · started 2026-06-25 · base origin/main @ 5d466ce (#178).
(Same session as [[cat-uutils-crate-free]] / #175; reuses the handle.)

## Goal
Retire td-feed's (gate 350) /gnu/store crate FODs. td-feed builds its own local HTTP
mirror (feed/) from source; it shares td-fetch's EXACT 73-crate closure (the recipe + old
lock literally reused td-fetch's). td-fetch already went guix-free (#172, own fetcher +
TD_VENDOR_DIR), so td-feed reuses td-fetch's td-fetched vendor — no cargo-proxy, no new warm.

## Key shape
- Reuse `.td-build-cache/crate-vendor/td-fetch` (73 crates, td-fetched by
  tools/warm-td-fetch-crates.sh, already in the check.sh prelude). NO new check.sh warm ⇒
  NOT an exclusive landing ⇒ affected-checks should WAIVE the full loop (gate 350 maps to
  the `td-feed` target).
- Supply-chain: each vendored sha ∈ feed/Cargo.lock (td-feed's OWN manifest; all 73
  checksums verified identical to fetch/Cargo.lock + present in feed/Cargo.lock).
- The index-TRUTHFULNESS leg (was: realized /gnu/store .crate FODs from the lock) is
  rerouted to iterate the td-fetched vendor crates; all 73 confirmed present in
  tests/td-feed.index (missing: 0).
- Dropped the verified-reproducible memoization (rm -rf scratch each run, always run
  td-builder check) to match the merged rust-fetch pattern — STRENGTHENS (always verifies
  repro), not a weakening.

## Sub-tasks
1. [done] Confirm td-feed shares td-fetch's closure (73==73, all ∈ feed/Cargo.lock) +
   crate-vendor/td-fetch warmed + all 73 in tests/td-feed.index.
2. [done] Rewrite gate 350 → guix-free (reuse vendor, intern source+vendor, TD_VENDOR_DIR,
   reroute index leg); keep selftest/cargo-proxy-selftest/index-consistency/stage0/repro.
3. [done] Strip tests/td-feed.lock to 7 toolchain-seed lines.
4. [done] affected-checks: warm-td-fetch-crates.sh → also td-feed; self-test assertion.
   self-test GREEN.
5. [in-progress] `./check.sh td-feed` GREEN.
6. [todo] Verified-red: corrupt a vendored crate → supply-chain leg fails; (and/or) the
   index-truthfulness leg.
7. [todo] affected-checks --committed-only --run (expect WAIVE, no check.sh edit) → land.
   Rebase onto main once #175 (cat-uutils) merges — only affected-checks overlaps (different
   regions).

## Notes
- feed-shared (gate 355) depends only on the built td-feed binary, not td-feed.lock's
  crate FODs — stripping the lock is safe for it.
- No recipe-td-feed.ts change (the recipe is vendoring-agnostic; minimal increment).
