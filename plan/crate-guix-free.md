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
(record per brick)
