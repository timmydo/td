section: mainline
status: done
handle: claude-opus-4b83d3
date: 2026-06-25
title: cat-uutils-crate-free
summary: Phase-2b follow-up — apply the corpus diverge (#169) to the uutils `cat` demo
  (rust-uutils gate, 340), the remaining corpus rust gate still carrying /gnu/store crate
  FODs. Migrate-in-place to the SHARED guix-free path (tests/crate-free-build.sh): the uu_cat
  0.9.0 source + its 139-crate closure are provisioned through td's OWN cargo-proxy
  (tools/warm-cargo-proxy.sh uu_cat 0.9.0 cat — uu_cat ships a Cargo.lock; proxy verifies each
  .crate sha256 == the crates.io index cksum), interned by store-add-recursive, built via
  TD_VENDOR_DIR (no guix build, no /gnu/store crate FOD, no oracle — content-address is the
  upstream pin). The gate's body becomes a crate-free-build.sh call + the cat file/stdin
  behavioral leg; tests/cat-uutils.lock is stripped to the 7 toolchain-seed lines (was 139
  crate FODs + cat-source FOD). CALLED OUT (directive 3): restructures 1 gate (340 body
  replaced, TD_VENDOR_CRATES→TD_VENDOR_DIR) + strips 1 lock; adds a check.sh prelude warm line
  (EXCLUSIVE spine landing) + an affected-checks mapping/self-test. Toolchain seed stays
  guix-built (retired last). Remaining guix-crate surface after this: td-feed(73, builds
  itself)/russh(188, local src)/td-ts-eval(128, seed)/td-vendor-demo(3). Notes in
  plan/cat-uutils-crate-free.md.
