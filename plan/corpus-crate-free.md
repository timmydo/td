# corpus-crate-free — working notes

Handle: claude-fable-65585b · claimed 2026-06-24

## Goal

Scale the **guix-free crate path** (engine B1/B2 + the cargo-proxy, all landed #163) to the
corpus rust gates so the shipped Rust userland builds with crates provisioned **with zero
guix** — and their locks DROP the `/gnu/store` crate strings. The toolchain seed (rust/gcc)
is the only remaining guix dependency, retired last by source-bootstrap.

Today each corpus rust gate realizes its crates via `guix build /gnu/store/<hash>-nv.crate`
(a guix-daemon FOD), enumerated from a per-package `.lock` that carries `/gnu/store` crate
strings. Replace that with the proxy-driven flow #163 proved on td-fetch.

## The generic mechanism (from #163, no per-package Cargo.lock work)

A host PREP, run before the offline build:

1. `td-feed cargo-proxy STORE ADDR` — start the cargo sparse-registry mirror (already built
   by the `td-feed` gate). cargo points at `sparse+http://ADDR/` via source replacement.
2. Fetch the package **source** crate guix-free: fetch the crates.io index for the package
   THROUGH the proxy (`/<index-path>`), parse the pinned version's cksum, `td-fetch`
   `proxy/dl/<name>/<version>/download` with that sha, extract → a source dir.
3. `cargo fetch --locked` in the source dir, `CARGO_HOME` clean, registry = the proxy → cargo
   resolves + fetches the WHOLE dep closure THROUGH the proxy; td verifies (sha==index cksum)
   + caches each `.crate` into the proxy's `crates/`. **That cache IS the vendor set.**
   (GOTCHA from #163: `cargo vendor` IGNORES source replacement; `cargo fetch`/`build` honor it.)
4. Intern source dir + the proxy's `crates/` as content-addressed trees (`store-add-recursive`,
   B2) → `build-recipe` with `TD_VENDOR_DIR` (B1), guix OFF PATH, no `guix build`.

No per-package `Cargo.lock` parsing for the deps (cargo does the resolution), no guix build,
no `/gnu/store` crate FOD, no oracle — content-address (the index cksum == the upstream pin)
is the equivalence proof (directive-4 refinement, same as #163).

## Why no oracle (directive-4 refinement, carried from #163)

Crates are content-addressed: "td's crate == guix's crate" is guaranteed by both matching the
crates.io index checksum (the upstream hash, NOT a guix artifact). Guix-free durable legs
replace the differential: supply-chain (fetched sha == index cksum), behavioral (the binary
runs), intrinsic-repro (`td-builder check` double-build), structural (guix off PATH / td store
/ no daemon / no `guix build` / **no `/gnu/store` crate path in the `.drv`**).

## Brick ladder

- [ ] **B1 — generic warm script** `tools/warm-cargo-proxy.sh NAME VERSION RECIPE` (host PREP):
  start the proxy, fetch+extract the source crate guix-free, `cargo fetch` the closure through
  it, leave source dir + vendor `crates/` under `.td-build-cache/crate-vendor/<name>/`. One
  script, every package.
- [ ] **B2 — ripgrep PoC gate** `rust-ripgrep-crate-free` (57 crates, smallest real corpus
  package): the warm script provisions it, the gate interns source+vendor + builds ripgrep via
  `TD_VENDOR_DIR`, asserts supply-chain + structural (no `/gnu/store` crate in `.drv`) +
  behavioral (`rg --version`) + repro. Verified-red each leg. This proves the GENERIC flow on a
  real corpus package end to end.
- [ ] **B3 — scale to the rest** (sd/fd/procs/eza/bat/uutils-cat/coreutils/youki/russh): one
  gate each, same script, drop each package's `/gnu/store` crate strings. Each a heavy
  from-source build validated individually + reproducible. (bat 207 crates, coreutils 507 —
  the heavy-validation cost is the remaining work; the warm is generic.)

## Brick status
- (starting) B1 generic warm script — IN PROGRESS.

## Verified-red evidence
(to fill as bricks land)
