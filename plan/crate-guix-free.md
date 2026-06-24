# crate-guix-free — working notes

Handle: claude-fable-65585b · claimed 2026-06-23

## Goal (human 2026-06-23)

Rust crate provisioning with **zero guix** — no guix-daemon, no guix process, no new guix
bytes, **and no guix differential oracle**. "I don't want any new guix dependencies, even
an oracle."

Today (rust-fetch et al.): `guix build /gnu/store/<hash>-nv.crate` (daemon FOD) → those
/gnu/store paths are TD_VENDOR_CRATES. Replace with: td-feed fetch (Cargo.lock-pinned) →
td-owned `store-add-recursive` of the crate SET as one content-addressed vendor tree →
`run_rust` vendors from it (TD_VENDOR_DIR). No /gnu/store crate strings.

## Why no oracle is OK (directive-4 refinement)

Crates are content-addressed: "td's crate == guix's crate" is guaranteed by both matching
the pinned **Cargo.lock checksum** (upstream crates.io hash, NOT a guix artifact). So the
guix differential is redundant. Guix-free proof legs replace it: supply-chain (fetched
sha == pin), behavioral (builds), intrinsic-repro (td-builder check), structural (guix off
PATH / td store / no daemon / no `guix build`). Surface this directive-4 refinement in the PR.

## Architecture

- The 73-crate set → **one** content-addressed vendor TREE (a dir of `*.crate`) interned by
  `store-add-recursive` (one call, one td store+db — sidesteps registering 73 paths).
- build-recipe exposes that interned tree to the sandbox + sets `TD_VENDOR_DIR`.
- `run_rust` vendors from `$TD_VENDOR_DIR/*.crate` (nv from filename; no name_from_store_path).
- Lock: crate entries become content-addressed `name + sha` (Cargo.lock); placement computes
  the td path. Toolchain seed lines stay /gnu/store (pre-existing, retired last).

## Brick ladder

- [ ] **B1 — `run_rust` vendors from `TD_VENDOR_DIR`.** build.rs: collect crate files from
  TD_VENDOR_CRATES (existing, store paths) AND/OR a new TD_VENDOR_DIR (a dir of `*.crate`,
  nv = filename). Additive — existing gates unchanged. Unit test (check-engine tier).
- [ ] **B2 — intern the crate set + expose it.** A PREP that assembles the feed-warmed
  crates into a vendor dir, `store-add-recursive` it, and a build-recipe path that exposes
  the interned tree + sets TD_VENDOR_DIR (extend build-recipe's input handling).
- [ ] **B3 — guix-free gate (PoC: td-fetch's 73 crates).** PREP: td-feed warm the crates
  (Cargo.lock-pinned, from a content-addressed lock) → intern → build-recipe with
  TD_VENDOR_DIR, guix OFF PATH, NO `guix build` → build td-fetch → assert supply-chain +
  behavioral + repro + structural. Verified-red each leg.
- [ ] **B4 — scale + lock cleanup.** Migrate the corpus rust locks to content-addressed
  crate entries (drop /gnu/store crate strings); regen from Cargo.lock.

## Verified-red evidence

### B1 (2026-06-23, commit 01526f5)
`collect_vendor_crates` unit test (`cargo test`, check-engine tier), perturbed:
- drop the `.crate` extension filter → `README.txt` leaks into the set → `assert_eq` fails.
- don't strip the `.crate` suffix → nv is `adler2-2.0.0.crate` not `adler2-2.0.0` → fails.
Restored → 58/58 builder tests green.

## Brick status
- B1 run_rust TD_VENDOR_DIR — DONE (commit 01526f5, verified-red, 58 tests green).
- B2 build-recipe exposes an interned vendor tree + sets TD_VENDOR_DIR — DONE (commit
  4a03da2, 58 tests). realize_drv's single src_override generalized to a slice of no-ref
  td-interned trees (source + vendor); build_recipe/assemble_recipe_drv take an optional
  vendor_store; the build-recipe subcommand grows [VENDOR-CANONICAL VENDOR-STORE VENDOR-DB].
  **PROVEN end-to-end** (throwaway `cgf` gate, now removed): stage0 rebuilt from this source,
  td store-add-recursive-interned td-fetch's 73-crate set and **built td-fetch from it via
  TD_VENDOR_DIR** — the .drv sets TD_VENDOR_DIR and references **NO /gnu/store crate path**
  (crates Cargo.lock-pinned), built by stage0 with guix off PATH, binary runs. The engine
  CAN now provision crates guix-free.
- B3 — DONE (commit 58d890d, `./check.sh rust-fetch-crate-free` GREEN). tools/warm-td-fetch-crates.sh
  (host PREP, wired into check.sh's prelude) td-fetches td-fetch's 73 crates from static.crates.io
  (Cargo.lock-pinned), the gate interns them as a vendor tree + builds td-fetch via TD_VENDOR_DIR.
  Durable legs all green: supply-chain (sha == Cargo.lock pin), structural (TD_VENDOR_DIR, NO
  /gnu/store crate path in the .drv), behavioral (runs), repro (td-builder check double-build).
  Verified-red: corrupt a crate → its sha not in Cargo.lock → supply-chain reds (isolated).
  NOTE: editing check.sh escalates landing to the FULL loop (heavy).
- **cargo-proxy — the generic B4 mechanism (human steer 2026-06-23: "build a proxy so cargo
  does the heavy lifting"). DONE + validated (commit 8e69f5f).** `td-feed cargo-proxy STORE
  ADDR` is a cargo SPARSE registry mirror: cargo (source replacement `sparse+http://ADDR/`)
  fetches its WHOLE closure THROUGH td — serve /config.json + proxy/cache the index + on
  /dl/CRATE/VERSION/download fetch static.crates.io, VERIFY sha256==index cksum, cache
  crates/NV.crate. So cargo does the resolution+fetch; td owns the verifying egress; the
  proxy's crates/ cache IS the vendor set B1/B2 intern. **GOTCHA: cargo `vendor` IGNORES
  source replacement (vendors canonical sources); cargo `fetch`/`build` HONOR it.** Hermetic
  selftest (mock upstream, verified-red) wired into the td-feed gate. Validated: cargo fetch
  through the proxy pulled all 73 of td-fetch's crates.
- B4 scale — NOW GENERIC via the proxy: PREP = start the proxy + `cargo fetch` the package
  through it (cargo resolves+fetches, any package, no per-package Cargo.lock work) → the
  proxy's crates/ cache → intern (B2) → build (B1). Remaining: a generic warm script + run the
  corpus rust gates (bat/fd/ripgrep/sd/eza/uutils/youki/russh) through it (each still a heavy
  from-source build to validate) + drop their /gnu/store crate strings. The proxy makes the
  WARM generic; the per-gate heavy validation is the remaining cost.
- LESSON (re-learned, [[td-commit-before-red-variants]]): `git checkout -- file` to revert a
  red perturbation WIPED the uncommitted cargo-proxy code (it was never committed). Commit
  green BEFORE perturbing for verified-red.
