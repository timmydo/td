# corpus-crate-free-rest — working notes

Handle: claude-fable-65585b · claimed 2026-06-24

## Goal (Phase 2a)

Finish OWNING the corpus rust packages guix-free, using the mechanism landed in #166
(`tools/warm-cargo-proxy.sh` + the shared `tests/crate-free-build.sh` helper). #166 did the
shipped Rust userland (procs/fd/ripgrep/sd/eza/bat). This does the remaining crates.io
packages: **uutils-coreutils** (crate `coreutils` 0.9.0, 507 crates, bin `coreutils`) and
**youki** (`youki` 0.6.0, 663 crates). Each gets an additive `rust-<p>-crate-free` gate; the
guix-path gates (343 rust-coreutils, 344 rust-youki) stay as the differential oracle.

russh (td-russh-demo, 188 crates) is a LOCAL demo source — NOT a crates.io package — so the
proxy throwaway-fetch can't grab its "source crate". Deferred (needs a source-from-disk warm).

PHASE 2b (the stated end goal) — once the whole corpus is owned guix-free, DROP the
`/gnu/store` crate strings from the locks + retire the guix-path crate FODs. Separate PR.

## Brick ladder

- [ ] B1 — uutils-coreutils crate-free gate (`rust-coreutils-crate-free`). warm
  `coreutils 0.9.0 uutils`; gate = crate-free-build.sh uutils coreutils-0.9.0
  tests/uutils-coreutils.lock uutils-source tests/ts/recipe-uutils.ts + behavioral
  (the multicall `coreutils` runs a util, e.g. `coreutils echo hi`). Heavy (507 crates).
- [ ] B2 — youki crate-free gate (`rust-youki-crate-free`). warm `youki 0.6.0`; gate =
  crate-free-build.sh youki youki-0.6.0 tests/youki.lock youki-source
  tests/ts/recipe-youki.ts + behavioral (`youki --version` / `--help`). Heavy (663 crates).
- prelude warms + affected-checks mappings for both.

## Brick status
- PROXY FIX (prereq): coreutils/youki need crates whose names start with "dl" (dlv-list via
  rust-ini->ordered-multimap). The cargo-proxy's `/dl/` download prefix COLLIDED with their
  sparse-index path `dl/v-/dlv-list` → 404. Fixed cargo_route (only `/dl/X/Y/download` is a
  download; else serve_index) + a selftest regression leg (verified-red). ALSO switched
  warm-cargo-proxy.sh's source-crate grab from a throwaway `cargo fetch` (which FRESH-resolves
  deps and fails on coreutils) to a direct proxy /dl GET (curl/wget — no resolution). Commit
  e8d6b0b. ripgrep still warms (57); coreutils warms (507), youki (663).
- B1 uutils — DONE. `./check.sh rust-coreutils-crate-free` GREEN: 507 crates, all durable legs
  (supply-chain ∈ Cargo.lock, structural TD_VENDOR_DIR + no /gnu/store crate, behavioral the
  multicall dispatches mkdir/cp/cat/ls/mv/rm, repro double-build).
- B2 youki — DONE. `./check.sh rust-youki-crate-free` GREEN: 663 crates, all durable legs
  (behavioral --version reports youki + --help lists the OCI `create` subcommand, repro).
- The crates.io corpus rust packages now ALL build guix-free (8 total: #166's six +
  uutils + youki). russh (td-russh-demo, LOCAL source) + Phase 2b (drop the /gnu/store crate
  strings) remain.

## Verified-red evidence
- **proxy dl-collision** (this PR): reverted cargo_route to the old erroring behavior → the
  cargo-proxy-selftest's new `dl`-prefixed-index leg reds (`/dl/te/dltest: bad download path` →
  404 → exit 1). Restored → selftest OK.
- supply-chain + structural legs are the SHARED tests/crate-free-build.sh, verified-red in #166
  via ripgrep (corruption + cross-path against the guix .drv); uutils/youki reuse it.
- behavioral legs inherited verbatim from the guix-path gates 343 (multicall dispatch) / 344
  (youki --version/--help); both binaries demonstrably do the behavior in the green run.
