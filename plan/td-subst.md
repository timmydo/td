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
- [x] **Inc2b+Inc3** — `subst/` binary (the networked + crypto half, shares feed's
      ureq+rustls/ring+sha2 closure): `keygen` / `sign` / `serve` / `fetch` / `selftest`.
      ed25519 (ring) signs each narinfo; `fetch` verifies the Sig against a pinned public
      key, then re-checks the nar's sha256 == NarHash. `selftest` is a self-contained
      loopback round-trip with three self-discrimination legs (tampered narinfo, corrupted
      nar, wrong key all red the fetch). Builds offline + selftest green. Verified-red below.
- [x] **Inc4a** — td-builder substitute-or-build consumer hook. `build-recipe`, after the
      cache-hit check, calls `try_substitute`: shells out to `td-subst fetch` (td-builder is
      dep-free), then `restore_substitute` (nar::read_nar + re-verify NarHash) + registers,
      writing the same registration + td.db a build writes → `CACHE=subst`. OFF unless
      `TD_SUBST_URL` is set (loop never sets it → directive 1 preserved). Verified-red below.
- [~] **Inc4b** — WRITTEN + committed (mk/gates/358-td-subst.mk + tests/td-subst.lock +
      recipe-td-subst.ts + subst/Cargo.lock pinned to td-feed's versions). The end-to-end
      demo logic is PROVEN locally with the real binaries (e2e: store-add-text → subst-export
      → sign → serve → fetch → nar-restore byte-identical + tamper-reject), and the
      from-source build half mirrors the proven td-feed gate. NOT yet seen green in-sandbox:
      `./check.sh td-subst` on a cold worktree rebuilds the whole corpus first (multi-hour) —
      CI on the warm image is the authoritative confirmation. Spec below.
- [ ] **Inc4b (spec)** — the durable end-to-end gate `mk/gates/<NNN>-td-subst.mk` + a
      `tests/td-subst.lock` (subst's vendored closure, == td-feed's) + a pinned
      `tests/td-subst.pub`: build a real recipe → `subst-export` its td.db closure → `sign`
      → `serve` on loopback → re-run `build-recipe` with `TD_SUBST_URL` in a FRESH scratch →
      assert `CACHE=subst` + the substituted output == the built output byte-identical + a
      tool from it runs; self-discrimination (tampered narinfo / wrong key → fall back to
      building). The heavy piece (builds subst from source like td-feed).
      NOTE: `subst-export DB STORE-DIR OUTDIR ROOT...` takes STORE-DIR as the directory
      holding each path FLAT as `<basename>` — `/gnu/store` for the live store, or a build's
      `newstore` (the same flat layout build_and_register / store-add-text write). PROVEN
      end-to-end locally: store-add-text → subst-export → sign → serve → fetch → nar-restore
      round-trips byte-identical + a tampered narinfo reds the fetch (logic for the gate's
      behavioral assertions).

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

### Inc2b+Inc3 (2026-06-24; perturb `subst/`, revert, reconfirm `selftest` OK)
`td-subst selftest` over loopback. Its three self-discrimination legs are load-bearing:
- **signature** — `verify_msg` returns `true` always → selftest died at the tampered-narinfo
  leg ("fetch ACCEPTED a tampered narinfo — the signature is not load-bearing", rc=1).
- **NarHash** — skip the `got != want` sha256 check → selftest died at the corrupted-nar
  leg ("fetch ACCEPTED a corrupted nar — the NarHash check is not load-bearing", rc=1).
(The wrong-key leg rides the same `verify_msg` path as the signature leg.)
Both reverted via `git checkout`; reconfirmed `selftest OK` rc=0.

### Inc4a (2026-06-24; perturb restore_substitute, revert, reconfirm green 63/63)
Test: `restore_substitute_round_trips_and_rejects_corruption` (corruption hits the file
CONTENTS, structure intact, so read_nar still parses → the NarHash check is the sole guard).
- **NarHash equality** — `if false && hash != want_hash` (skip the check) → the test red
  ("restore accepted a nar whose contents do not match the signed NarHash"); the happy-path
  legs stayed green, so the guard is isolated. Reverted via `git checkout`; reconfirmed green.
(Loop-safety — `TD_SUBST_URL` unset → `try_substitute` returns None immediately — is covered
by the full builder suite still passing 63/63 with the env unset, the loop's actual state.)
