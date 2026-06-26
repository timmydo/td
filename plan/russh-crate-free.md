# russh-crate-free — working notes

Handle: claude-opus-4b83d3 · started 2026-06-25 · base origin/main @ 159168d (#181).
(Same session as [[cat-uutils-crate-free]] / [[td-feed-crate-free]]; reuses the handle.)

## Goal
Retire the rust-russh gate's (345) /gnu/store crate FODs. td-russh-demo is the LOCAL-SOURCE
case (in-repo tests/russh-demo/, not a crates.io crate) with the FULL crates.io closure (188
crates incl. aws-lc C crypto). This needed the one genuinely-new mechanism: a source-from-disk
warm that proxy-fetches the DEP closure from the in-tree Cargo.lock.

## New mechanism: tools/warm-cargo-proxy-local.sh SRCDIR DEST
Like warm-cargo-proxy.sh but for an in-tree source: NO source-crate download/extract — just
start td's cargo-proxy and `cargo fetch --locked` (fresh CARGO_HOME) in SRCDIR, publish the
proxy's verified crate cache to .td-build-cache/crate-vendor/DEST. The source tree stays the
in-repo dir (interned by the gate at gate time). Standalone (not a flag on warm-cargo-proxy.sh)
so editing it only triggers rust-russh in affected-checks, not all 8 corpus gates.

## Sub-tasks
1. [done] Confirm russh-demo ships Cargo.lock + all 188 deps are crates.io (no git/path) +
   1 no-source pkg (the local crate). VERIFIED.
2. [done] Write tools/warm-cargo-proxy-local.sh; tested → 188 crates provisioned guix-free,
   all 188 shas ∈ tests/russh-demo/Cargo.lock.
3. [done] Migrate gate 345 → guix-free (intern local source + vendor tree, TD_VENDOR_DIR,
   supply-chain vs russh-demo/Cargo.lock; keep SSH round-trip behavioral + repro).
4. [done] Strip tests/td-russh-demo.lock to 7 toolchain-seed lines.
5. [done] check.sh heavy_warm block: + warm-cargo-proxy-local.sh tests/russh-demo russh.
6. [done] affected-checks: warm-cargo-proxy-local.sh + tests/russh-demo/* -> rust-russh; self-test.
   self-test GREEN.
7. [in-progress] ./check.sh rust-russh GREEN.
8. [todo] Verified-red: bogus extra crate in the vendor -> supply-chain leg fails fast (the
   warm-proof method, since the cargo-proxy warm has no self-heal but a tracked-file tamper can be
   clobbered — bogus crate is cleanest). NOTE: warm-cargo-proxy-local skips if vendor already
   populated, so a bogus extra crate survives it.
9. [todo] Full loop (affected-checks escalates on the check.sh edit) -> land per protocol.

## Notes
- aws-lc-sys has a C build script — the build env (CC/CXX + C_INCLUDE_PATH from gcc-toolchain,
  which bundles the kernel headers) is provided by run_rust; NO extra seed (cmake/linux-headers
  were NOT actually needed — the old lock header overstated it; gcc-toolchain/include suffices).
- No recipe-td-russh-demo.ts change (vendoring-agnostic; minimal increment).
- After this: only td-ts-eval (seed) + td-vendor-demo (keep for TD_VENDOR_CRATES coverage) carry
  crate FODs. The toolchain seed (7 rust/gcc lines) is source-bootstrap's job.
