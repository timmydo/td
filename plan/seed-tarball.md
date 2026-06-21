# seed-tarball — North-Star step 2: the frozen toolchain seed

Handle: claude-fable-db65ca · branch: seed-tarball

## Goal

Serve the toolchain SEED (gcc/glibc/binutils + the build tools td can't yet
self-build) from a **frozen, pinned binary tarball**, so the loop builds with **no
guix install** — closing the step-1 boundary (where `td shell`/`build-recipe` link
the guix-built seed from the lock and stage it out of the live `/gnu/store`).

## Mechanism (grounded in existing td-builder primitives)

- **Capture** (`tools/build-seed-tarball.sh NAME…`, one-time on a guix host — like a
  channel bump, NOT in the loop): resolve seed names → store paths; union their
  closures (`td-builder store-closure /var/guix/db/db.sqlite PATH`); `tar` the closure
  trees (canonical `/gnu/store/<base>`); write a **manifest** — per closure member:
  `<path> <nar-hash> <ref>…` (refs from the store DB, nar-hash from `td-builder
  nar-hash`). Tarball + manifest are pinned content-addressed (warmed like the tsgo
  tarball; a `tests/td-seed.lock` records the path/hash). Regenerate deliberately.
- **Unpack** (`td-builder seed-unpack TARBALL MANIFEST DEST-STORE DEST-DB`): extract
  the trees into DEST-STORE (`DEST-STORE/<base>`), then write DEST-DB registering each
  canonical path with its refs + nar-hash **from the manifest** — no re-scan (the live
  `/gnu/store` is read-only in the loop, so registration can't scan there; the manifest
  carries the refs/hashes captured at capture time). Verify each restored tree's
  `nar-hash` == the manifest (NAR-identical round-trip). No daemon, no `/gnu/store` write.
- **Build from the seed** (PR2): `build_recipe` already stages the input closure from
  the passed `store_dbs` + `td_store` (the realize multi-DB staging). Point those at
  DEST-DB + DEST-STORE so a build's seed inputs come from the tarball, with `/var/guix`
  and the live `/gnu/store` seed paths OUT of the path.

## PRs

1. **Capture + unpack round-trip** (this PR): the capture tool, the `seed-unpack`
   subcommand, and a gate that captures hello's seed closure → unpacks into a FRESH td
   store → asserts every path is NAR-identical to the manifest (the seed survives the
   tarball) and DEST-DB is closure-complete (durable, no guix oracle needed). The guix
   store DB is the capture SOURCE (the seed comes from guix once, by design) — a
   removable leg is "the captured nar-hash == the live store's".
2. **Build hello from the unpacked seed** — `td shell hello`/`build-recipe` staging the
   seed from DEST-STORE/DEST-DB, gate run with `/var/guix` + the live seed paths made
   unavailable, proving the build needs only the tarball. The real "no guix install" demo.
3. Wire the locks/cache-lib to resolve seed paths from the tarball manifest (the corpus
   gates build from the seed store), and pin + warm the tarball (`tests/td-seed.lock`,
   a `warm-seed.sh` like `warm-tsgo.sh`).

## Status

- 2026-06-21: PR1 (capture) green — `tools/build-seed-tarball.sh` + the `seed-tarball`
  gate. Captured bash's 6-path closure (incl. glibc, 92M tar); NAR-identical after a
  tar round-trip; complete; td closure == `guix gc -R`. No Rust change (reuses
  store-closure + nar-hash). Verified-red: VR-A (drop a captured path → "incomplete
  capture" reds), VR-B (corrupt a manifest hash → "NAR mismatch after round-trip" reds).
- 2026-06-21: PR2 (unpack) green — `td-builder seed-manifest` (richer manifest: path,
  nar-hash, nar-size, refs) + `td-builder seed-unpack` (extract + NAR-verify + register
  DEST-DB from the manifest, no daemon, no /gnu/store write); new `seed-unpack` gate. td's
  own store-closure reads the COMPLETE closure back out of the unpacked DB. Verified-red:
  VR1 (corrupt a manifest hash → seed-unpack rejects "NAR mismatch after restore"), VR2
  (skip Refs → "incomplete registration"). PR1 capture gate updated for the 4-field manifest.
- 2026-06-21: PR3 (build-from-seed) GREEN — the payoff. `build-recipe` gains a seed-store
  override (TD_SEED_STORE + TD_SEED_DB); the new `seed-build` gate captures hello's full
  build closure (54 paths, 660M), seed-unpacks it, and builds hello passing the seed DB as
  its ONLY store DB — hello builds + RUNS "Hello, world!" with /var/guix + the live
  /gnu/store out of the build path; every input stages from the unpacked store; the
  seed-built hello == the guix-seed build (same drv). Verified-red: VR1 — build from an
  incomplete seed FAILS (no guix fallback; the build is bounded by the seed).
  **Step 2 demo complete: td builds with no guix install.**
- 2026-06-21: warm + pin — `tools/warm-seed.sh` captures + seed-unpacks the seed ONCE into a
  content-addressed cache and reuses it (no 660M re-capture per run); `tests/td-seed.lock`
  pins hello's seed manifest hash and the seed-build gate asserts the warmed seed matches it
  (DURABLE repro: the toolchain seed is reproducible + channel-anchored). Verified-red: a
  corrupted pin reds the repro leg.
- Remaining for full guix independence (later): route `td shell` + the corpus build onto the
  warmed seed (so the WHOLE loop builds with no guix install); retire the guix oracle/lowering
  (step 3, last).
