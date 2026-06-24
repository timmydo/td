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
- B1 generic warm script `tools/warm-cargo-proxy.sh` — DONE. Validated on ripgrep: throwaway
  project `cargo fetch` grabs the source crate THROUGH the proxy; extract; clear the proxy
  cache + a FRESH cargo home; `cargo fetch --locked` in the source pulls the full 57-crate
  closure THROUGH the proxy (the fresh cargo home is essential — a reused one cache-hits 9
  crates and they never reach the proxy → an incomplete vendor set). Source + 57 `.crate` left
  in `.td-build-cache/crate-vendor/ripgrep/{src,vendor}`.
- B2 ripgrep PoC gate `355-rust-ripgrep-crate-free.mk` — DONE, `./check.sh
  rust-ripgrep-crate-free` GREEN. All durable legs: supply-chain (57 vendored sha ∈ ripgrep's
  shipped Cargo.lock == crates.io cksum), structural (interned source+vendor, .drv has
  TD_VENDOR_DIR + NO /gnu/store crate path), behavioral (rg greps a needle, not the unrelated
  file), repro (td-builder check double-build). Warmed via the check.sh prelude
  (`warm-cargo-proxy.sh ripgrep 14.1.1`). NOTE: editing check.sh escalates landing to the FULL
  loop (heavy).
- REFACTOR (for scale): the shared guix-free build+assert (supply-chain, intern source+vendor,
  build-recipe via TD_VENDOR_DIR, structural, repro) is now ONE helper `tests/crate-free-build.sh`
  (prints OUT=/NS=); each gate is thin — warm-check + `crate-free-build.sh NAME CRATEDIR LOCK
  SOURCEKEY RECIPE` + its package-specific behavioral leg. warm-cargo-proxy.sh grew an optional
  DEST arg (crates.io name != recipe name: `fd-find`->fd, `coreutils`->uutils).
- B3 scale — sd (111 crates, name matches) + fd (113 crates, name DIFFERS: crate `fd-find`) added
  as `rust-sd-crate-free` / `rust-fd-crate-free`. Remaining: procs(297)/eza(233)/bat(207)/
  uutils-cat+coreutils(507)/youki(663)/russh(188, LOCAL demo source — not a crates.io pkg, needs
  a different source path). Each a heavy from-source build; the warm + helper are generic, so each
  is ~10 lines of gate + 1 prelude warm line.
- DIVERGE (Phase 2, follow-on): these `-crate-free` gates are ADDITIVE — the guix-path gates
  (347/349/346/...) stay as the differential oracle while the corpus is being OWNED guix-free
  ("own, then diverge"). Once the whole corpus has a green crate-free gate, retire the guix-path
  crate FODs + DROP the /gnu/store crate strings from the locks (the stated end goal).

## Verified-red evidence

### B2 ripgrep PoC (2026-06-24)
- **supply-chain** — append a byte to a vendored `.crate` → its sha256 no longer ∈ ripgrep's
  Cargo.lock → the gate's exact loop reds (`miss=1`). The leg catches a crate whose bytes are
  not the upstream pin.
- **structural / TD_VENDOR_DIR required** — ran `grep -q TD_VENDOR_DIR` against the GUIX-PATH
  ripgrep `.drv` (`.td-build-cache/rust-ripgrep/b/ripgrep-14.1.1.drv`, which uses
  TD_VENDOR_CRATES): RED (it lacks TD_VENDOR_DIR). The leg discriminates the guix-free build.
- **structural / no /gnu/store crate path** — ran the gate's
  `grep -oqE '/gnu/store/...\.crate'` against the same guix-path `.drv`: RED (it HAS
  `/gnu/store/...grep-searcher-0.1.14.crate` etc.). The guix-free assertion has teeth — it
  separates the guix FOD path from the proxy/vendor path.
- **behavioral + repro** — same logic as rust-ripgrep (347), verified-red there (needle match
  + over-match guard; td-builder check double-build).

### B3 sd + fd (2026-06-24)
- The supply-chain + structural + repro legs are now in the SHARED `tests/crate-free-build.sh`,
  verified-red via the ripgrep PoC above (corruption + cross-path against the guix `.drv`) —
  sd/fd run the identical code, so those legs are covered for them.
- behavioral legs are copied verbatim from rust-sd (349) / rust-fd (346), verified-red in their
  history (sd replace + non-match-unchanged; fd find + pattern-not-leaked). Both binaries
  demonstrably DO the behavior in the green run.
- All three green: `./check.sh rust-ripgrep-crate-free` (57), `rust-sd-crate-free` (111),
  `rust-fd-crate-free` (113) — each exit 0, binary built, all 5 durable legs printed.
- NOTE on running them: `make check` runs every heavy gate (they're prereqs of the ONE `check`
  goal). A manual `./check.sh A B C` with MULTIPLE goals only runs the first under
  `-j2 --output-sync` — validate crate-free gates one target per `./check.sh` invocation.
