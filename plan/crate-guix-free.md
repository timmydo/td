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
- B3 — productionize into a COMMITTED gate. The remaining piece is guix-free crate SOURCING:
  the cgf proof sourced crate bytes from the realized crates (verified vs fetch/Cargo.lock);
  a committed guix-free gate must fetch them via td-feed (a host-PREP warm of td-fetch's
  Cargo.lock-derived crate index into a vendor dir), then intern + build + assert
  (supply-chain pin / behavioral / repro / structural) with no guix in the gate. Verified-red.
- B4 scale (the corpus rust locks → content-addressed crate entries, drop /gnu/store crate
  strings; the rust-* gates switch to the vendor-tree path) — after B3.
