# td-subst — working notes

Handle: claude-opus-6fda3a · claimed 2026-06-24 · branch worktree-td-subst

## Goal
td's own substitute (binary-cache) server: serve BUILT `/td/store` outputs as
NAR + signed metadata so `td install` / CI image prep / a cold worktree can fetch
a path instead of rebuilding it. The dual of td-feed (which mirrors *source*
downloads). DESIGN §5/§6 park this for "the era when td runs its OWN builder
daemon" — the primitives already exist in td-builder.

## Design decisions (human, 2026-06-24)
- **Consumer = all of the above, one server + one consumer mechanism**: `td install`,
  CI provisioning, worktree cache. HARD LINE: the verification loop NEVER substitutes
  (directive 1 — it builds from source + `--check`). The consumer is OFF for the loop,
  opt-in elsewhere; the gate exercises it over loopback, isolated.
- **Protocol = td-native minimal** (NOT guix narinfo): feed-style, index-free — a small
  signed metadata file per path (`path|narhash|narsize|refs`) + a `/nar/<…>` endpoint.
- **Trust = signed + repro-equality durable leg**: ed25519 (via already-vendored `ring`)
  signs the metadata; consumer verifies against a PINNED td public key
  (`tests/td-subst.pub`; private key held by the publisher, never committed). The gate's
  DURABLE assertion: a path fetched into a clean store == a from-source rebuild,
  byte-identical, the closure `store-verify`s, and a tool from it runs.
- **First PR = server + td-native consumer.**

## Why most of it already exists
- `nar::write_nar` — NAR body, guix-byte-compatible (the wire payload).
- `store::output_path` / `make_store_path` — input-addressed output-path hash (the key).
- `store-query info` → NarHash/NarSize; `store-query references` + `store-closure` →
  the metadata `refs` + the recursive fetch set.
- feed/src/main.rs — the std::net HTTP server + atomic-write + verify-on-serve +
  selftest-with-self-discrimination pattern to copy.
- `ring` is already vendored (rustls in fetch/feed) → ed25519 is free.

## Architecture refinement (2026-06-24) — the dependency boundary
`td-builder` has **zero crate dependencies** (pure std — the cargo-test gate relies on it;
adding `ring` would bloat its from-source closure ~40 crates and break `--frozen` with no
deps). ed25519 signing needs `ring`. So split along that boundary, the same way the repo
already splits the pure-std store engine from the networked `fetch`/`feed`:
- **td-builder (no new deps)** owns the *store-coupled* half — it already has nar
  (read+write), store_db_read, store-closure, store::output_path. New: `subst-export`
  produces a serve-able directory from a store path + its closure: a td-native
  `<pathhash>.narinfo` per path (StorePath/NarHash/NarSize/References) + `nar/<narhash>.nar`.
  Plus the `nar-restore` consumer primitive (Inc1, done).
- **a separate `subst/` binary** (shares feed/fetch's `ureq+rustls/ring+sha2` closure)
  owns the *network+crypto* half: `sign` the narinfos (ed25519), `serve` the dir
  (std::net, verify-on-serve), `fetch` (verify sig + nar hash, then hand to
  `td-builder nar-restore` + store-register). This is the piece with a heavy from-source
  BUILD_GATE (like td-feed); td-builder's half rides the existing stage0 build.

This keeps the loop's verification engine dependency-free and confines the crypto/HTTP
surface to the networked tool — and makes Inc2 (subst-export) unit-testable with NO
network and NO new deps.

## The one substantial new primitive
`nar::read_nar` — the inverse of `write_nar`. Restores a NAR stream to a path on disk
(regular/executable file, symlink, directory tree). Format is fully specified in
nar.rs's header comment. Needed by the consumer to unpack a fetched substitute.

## Increment ladder
- [x] **Inc1** — `nar::read_nar` (inverse of write_nar) + `nar-restore NARFILE DEST`
      CLI (the read side, wired so it isn't dead code) + 3 verified-red unit tests.
- [x] **Inc2** — `td-builder subst-export DB STORE OUTDIR ROOT...`: the store-coupled,
      dependency-free half — writes `<basename>.narinfo` + `nar/<narhash>.nar` per closure
      member (reuses nar::write_nar + store_db_read + Refs closure). Verified-red below.
- [ ] **Inc2b** — `subst/` binary: `serve` / `fetch` / `selftest`, loopback green
      (serve an exported dir → fetch → read_nar unpack → byte-identical). Networked half.
- [ ] **Inc3** — ed25519 signing of the metadata + pinned-key verify on the consumer;
      self-discrimination legs (corrupt NAR, bad signature, tampered narhash all red).
- [ ] **Inc4** — td-builder consumer hook (substitute-or-build before
      build_and_register, OFF for the loop) + the durable fetch-instead-of-build gate
      `mk/gates/<NNN>-td-subst.mk`.

## Verified-red evidence
(record per-increment here)

### Inc1 (2026-06-24, all reverted after observing red; final state green 61/61)
Three targeted single-point perturbations, each isolating exactly one new assertion:
- **read_nar_round_trips_a_tree** — set the restored mode to 0o644 always (ignore the
  executable bit) → the re-serialized NAR drops the "executable" token → ONLY this test
  red (panicked at the exec-bit assertion); other 5 green.
- **read_nar_rejects_a_truncated_archive** — `let _ = read_node(..); Ok(())` (tolerate a
  partial restore) → ONLY this test red (read_nar returned Ok on a truncated stream);
  other 5 green.
- **read_nar_rejects_bad_magic** — skip the magic-token check → ONLY this test red (a
  valid body behind a wrong magic restored); other 5 green.
Each reverted via `git checkout -- builder/src/nar.rs` (Inc1 was committed first, so the
revert restored the green state without losing work). Re-confirmed green after each.

### Inc2 (2026-06-24; perturb subst_export, revert, reconfirm green 62/62)
Test: `subst_export_writes_narinfos_and_restorable_nars`.
- **References basenames** — record refs as full `/gnu/store/...` paths instead of
  basenames → the `References: <basename>` assertion red (printed the full path).
- **NarHash truth + round-trip** — record `sha256:deadbeef` instead of the real nar hash
  → the `assert_eq!(narhash, real)` leg red (`deadbeef` != the true hash).
  (A first attempt — serialize a *different* path's nar — was discarded: pointing
  write_nar at the nar output dir is self-referential and HANGS rather than failing
  cleanly; the constant-wrong-hash perturbation is the clean equivalent.)
Both reverted via `git checkout`; reconfirmed green.
