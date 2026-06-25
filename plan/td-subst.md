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

## The one substantial new primitive
`nar::read_nar` — the inverse of `write_nar`. Restores a NAR stream to a path on disk
(regular/executable file, symlink, directory tree). Format is fully specified in
nar.rs's header comment. Needed by the consumer to unpack a fetched substitute.

## Increment ladder
- [ ] **Inc1** — `nar::read_nar` + a verified-red round-trip test
      (write_nar(dir) → read_nar → identical tree; + a self-discrimination control:
      a truncated/garbled NAR errors). VERIFY RED FIRST.
- [ ] **Inc2** — `subst/` binary: `publish` / `serve` / `selftest`, loopback green
      (publish a tiny path → serve → fetch → read_nar unpack → byte-identical).
- [ ] **Inc3** — ed25519 signing of the metadata + pinned-key verify on the consumer;
      self-discrimination legs (corrupt NAR, bad signature, tampered narhash all red).
- [ ] **Inc4** — td-builder consumer hook (substitute-or-build before
      build_and_register, OFF for the loop) + the durable fetch-instead-of-build gate
      `mk/gates/<NNN>-td-subst.mk`.

## Verified-red evidence
(record per-increment here)

### Inc1
- (pending)
