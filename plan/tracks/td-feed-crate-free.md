section: mainline
status: done
handle: claude-opus-4b83d3
date: 2026-06-25
title: td-feed-crate-free
summary: Phase-2b follow-up — apply the diverge to td-feed (gate 350), the td-built local
  HTTP mirror. td-feed shares td-fetch's crate closure EXACTLY (ureq+rustls/ring+sha2, 73
  crates — only the bin name differs) and td-fetch is already guix-free (#172), so td-feed
  reuses td-fetch's td-fetched vendor tree (.td-build-cache/crate-vendor/td-fetch, warmed by
  tools/warm-td-fetch-crates.sh — NO new check.sh warm line, so NOT an exclusive landing).
  Gate 350: realize ONLY the toolchain seed, intern the local feed/ source + the vendor tree
  (store-add-recursive), build-recipe with TD_VENDOR_DIR (was TD_VENDOR_CRATES + guix-build
  the 73 FODs); the index-TRUTHFULNESS leg is rerouted from the lock's /gnu/store FODs to the
  td-fetched vendor crates (all 73 still in tests/td-feed.index). All of td-feed's extra
  durable legs kept (selftest warm/serve/fetch, cargo-proxy-selftest, index self-consistency,
  stage0-builder, repro). tests/td-feed.lock stripped to the 7 toolchain-seed lines (was 73
  crate FODs). Supply-chain checks each vendored sha ∈ feed/Cargo.lock (td-feed's OWN
  manifest; all 73 verified ∈ it). CALLED OUT (directive 3): restructures gate 350 (body,
  TD_VENDOR_CRATES→TD_VENDOR_DIR, index leg rerouted) + strips 1 lock + an affected-checks
  mapping/self-test. NO check.sh edit (reuses td-fetch's warm). Toolchain seed stays
  guix-built (retired last). Remaining guix-crate surface after this: td-russh-demo(188, LOCAL
  src — needs source-from-disk warm)/td-ts-eval(128, seed)/td-vendor-demo(3, KEEP to exercise
  the TD_VENDOR_CRATES path). Notes in plan/td-feed-crate-free.md.
