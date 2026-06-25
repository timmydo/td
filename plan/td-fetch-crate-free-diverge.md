# td-fetch-crate-free-diverge — working notes (Phase 2b for td-fetch)

Handle: claude-fable-65585b · claimed 2026-06-24

## Goal

The corpus diverge (#169) retired the guix-path crate FODs for the 8 corpus packages but left
td-fetch out of scope. td-fetch was in the EXACT pre-Phase-2b state: both rust-fetch (348,
guix-path, TD_VENDOR_CRATES) and rust-fetch-crate-free (354, TD_VENDOR_DIR) existed. Apply the
identical migrate-in-place so the canonical `rust-fetch` builds td-fetch's crates guix-free and
td-fetch.lock loses its 73 /gnu/store crate strings.

## What's different from the corpus

- td-fetch's SOURCE is the LOCAL `fetch/` dir (interned via intern-src.sh), NOT a crates.io
  crate — so td-fetch.lock has no `*-source` FOD line to drop (only the 73 `.crate` dep FODs).
- td-fetch's crates come from `tools/warm-td-fetch-crates.sh` (td-fetch's OWN fetcher, NOT the
  cargo-proxy), already in the check.sh prelude — so NO new warm line.
- 354 is a BESPOKE gate body (predates tests/crate-free-build.sh, from #163), so the migration
  is `sed rust-fetch-crate-free -> rust-fetch` of 354 into 348 (no helper involved).

## Change
- `sed 's/rust-fetch-crate-free/rust-fetch/g' 354 > 348`; `git rm` 354. Removed the now-stale
  "Contrast rust-fetch (348)…guix build" self-reference from the doc header.
- Stripped tests/td-fetch.lock to 7 toolchain lines (0 crate FODs) + a td-fetch-specific header.
- affected-checks: `warm-td-fetch-crates.sh -> rust-fetch` (was rust-fetch-crate-free); self-test
  assertion updated. Stale `rust-fetch-crate-free` comments in check.sh + warm-td-fetch-crates.sh
  fixed to `rust-fetch`.

## Out of scope (still guix-FOD crates)
cat-uutils (rust-uutils, 139), td-feed (73, builds itself), russh (188, local src — needs a
source-from-disk warm), td-ts-eval (128, seed), td-vendor-demo (3). rust-vendor/rust-ts-eval keep
TD_VENDOR_CRATES coverage.

## Brick status
- Migrated (sed body move + git rm 354 + strip lock + doc/comment cleanup); self-test green.
- VALIDATING: `./check.sh rust-fetch` (the migrated canonical gate builds td-fetch guix-free from
  the stripped lock; durable legs supply-chain ∈ fetch/Cargo.lock, structural TD_VENDOR_DIR + no
  /gnu/store crate, behavioral runs, repro).

## Verified-red
The durable legs are unchanged from 354 (verified-red in #163: corrupt a crate -> sha not in
fetch/Cargo.lock -> supply-chain reds). The migration is a body-move + lock-strip; the stripped
lock has 0 crate strings (grep -c '\.crate ' tests/td-fetch.lock == 0).
