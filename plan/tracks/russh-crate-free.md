section: mainline
status: done
handle: claude-opus-4b83d3
date: 2026-06-25
title: russh-crate-free
summary: Phase-2b follow-up — apply the diverge to td-russh-demo (rust-russh gate 345, a russh
  0.61 client<->server SSH round-trip incl. the aws-lc C crypto backend, 188 crates). This is
  the LOCAL-SOURCE case: the td-russh-demo crate is in-repo (tests/russh-demo/), not a crates.io
  crate, so it needed a NEW source-from-disk warm: tools/warm-cargo-proxy-local.sh SRCDIR DEST
  starts td's cargo-proxy and cargo-fetches ONLY the dep closure through it from the in-tree
  Cargo.lock (no source-crate download; the gate interns the live repo source). Migrate gate 345
  like td-feed/rust-fetch: intern local source + the warmed vendor tree (store-add-recursive),
  build-recipe with TD_VENDOR_DIR (was TD_VENDOR_CRATES + guix-build the 188 FODs); supply-chain
  checks each vendored sha ∈ tests/russh-demo/Cargo.lock; behavioral keeps the full SSH
  handshake/auth/exec round-trip (td-russh-ok: ping); repro double-build (always). tests/
  td-russh-demo.lock stripped to the 7 toolchain-seed lines (was 188 crate FODs). CALLED OUT
  (directive 3): NEW tools/warm-cargo-proxy-local.sh; restructures gate 345 (TD_VENDOR_CRATES->
  TD_VENDOR_DIR, dropped the repro memoization so it always double-builds = strengthening) +
  strips 1 lock; adds a check.sh prelude warm line in the heavy_warm block (EXCLUSIVE spine) +
  affected-checks mappings/self-test. Toolchain seed stays guix-built (retired last). After this
  the only FOD-carrying locks are td-ts-eval (seed) + td-vendor-demo (KEEP — exercises
  TD_VENDOR_CRATES). Notes in plan/russh-crate-free.md.
