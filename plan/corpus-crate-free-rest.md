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
- (starting) warm uutils + youki kicked off (host PREP).

## Verified-red evidence
(shared-helper legs were verified-red in #166 via ripgrep — corruption + cross-path; these
gates reuse the identical helper. Behavioral legs inherited from the guix-path gates 343/344.)
