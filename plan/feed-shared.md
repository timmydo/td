# feed-shared — working notes

Handle: claude-fable-65585b · claimed 2026-06-23 · follows #157 (td-feed landed)

## Goal

A SHARED, persistent, offline td-feed that serves the loop's td-NATIVE fetch egress
(tsgo tarball + bootstrap source tarballs requested by td-fetch / tools/warm-*.sh) across
many agents/worktrees, so those downloads happen ONCE into a shared store.

## Decisions (human, 2026-06-23)

1. **td-native scope only** — feed serves td-fetch's artifacts; the guix daemon keeps its
   own crate/tarball FODs. The 1838 crates in tests/td-feed.index are catalog-only,
   daemon-served, NOT re-warmed by the feed (they're already in the shared /gnu/store).
2. **Persistent host daemon + shared store** — one `td-feed serve` on the host, store at
   `~/.td/feed` (`TD_FEED_DIR`), fixed loopback addr, started-if-not-running.
3. **Regenerate committed index + warm delta** — no runtime config mutation.
4. **Auto-warm in check.sh prelude** — idempotent; export TD_FEED_BASE for the loop.

## Design

- Shared dir `${TD_FEED_DIR:-~/.td/feed}`: `store/` (artifacts + `.sha256` sidecars),
  `feed.addr`, `feed.pid`, `feed.lock`.
- **Sidecar serve** (index-free): `td-feed serve STORE ADDR` → `GET /<path>` reads
  `store/<path>` + `store/<path>.sha256`, verifies, serves. Self-describing store → a
  persistent daemon serves any branch's warmed artifacts with no index coupling.
- **warm** = supply-chain gate: `td-feed warm INDEX STORE` fetches each upstream URL,
  verifies vs the PINNED index sha, writes file + sidecar (idempotent).
- **serve** = integrity gate: verify file vs its sidecar (catches store corruption).
- **tools/feed-ensure.sh**: flock(feed.lock); reuse if pid+addr live, else start the
  daemon detached on 127.0.0.1:0, record addr/pid; prints the addr.
- **Reroute**: warm-tsgo.sh / warm-bootstrap-sources.sh inherit TD_FEED_BASE (already
  td-fetch-based; td-fetch honors it since #157).

## Brick ladder

- [ ] **Inc1a — sidecar serve/warm.** Change td-feed: warm writes `<path>.sha256`
  sidecars; serve verifies vs sidecar (drop the INDEX arg). Update selftest + the #157
  gate (mk/gates/350-td-feed.mk). Verified-red: corrupt file → serve 500; wrong pin → warm.
- [ ] **Inc1b — tools/feed-ensure.sh** + a `feed-shared` gate: start a shared daemon in a
  temp TD_FEED_DIR, warm an artifact, a SECOND consumer (simulated other worktree) reads
  it offline via TD_FEED_BASE; verified-red (cold path 404s; corrupt store reds).
- [ ] **Inc2 — check.sh prelude wiring** (SPINE, exclusive landing, full loop): feed-ensure
  + warm td-native subset + export TD_FEED_BASE; add bootstrap sources to the index
  (gen-feed-index.sh scans seed/sources/*.lock). The warm scripts then read from the feed.

## Verified-red evidence

### Inc1 (2026-06-23, commit d835e27 — both gates GREEN via ./check.sh)
- **td-feed gate** (rebuilt from the sidecar source): `selftest` passes; verified-red built
  into the binary — wrong pinned hash reds warm, corrupted store byte reds serve (sidecar).
- **feed-shared gate**: one shared daemon (feed-ensure); two consumers in different cwds
  fetched the same blob through it offline; a 2nd ensure reused the same addr+pid;
  SELF-DISCRIMINATION — corrupting the shared store reds the consumer (serve sidecar), a
  cold path 404s. Host-validated the same legs first; the bug found+fixed: the detached
  daemon inherited the flock fd and deadlocked later ensures (now `9>&-`).

## Brick status
- Inc1a sidecar serve/warm — DONE (td-feed gate green).
- Inc1b feed-ensure + feed-shared gate — DONE (feed-shared gate green).
- Inc2 wiring — DONE. `tools/warm-bootstrap-sources.sh` routes each bootstrap tarball
  through the shared feed: feed-ensure → `td-feed warm` into ~/.td/feed (egress only if
  cold) → `td-fetch` reads it via TD_FEED_BASE (offline), with a direct-td-fetch fallback.
  No check.sh spine edit (the warm script self-bootstraps the daemon via feed-ensure).
  **Validated** (`./check.sh td-feed feed-shared bootstrap-mes bootstrap-mescc
  bootstrap-tcc` all GREEN, full loop waived): the gate prelude warmed mes/nyacc/tcc via
  the shared feed (~/.td/feed) and the bootstrap chain built mes/mescc/tcc from them.
  Host-validated the sharing: a 2nd worktree (cold sources, warm feed) got all three
  OFFLINE from the shared feed; the cold feed egresses once, then shares.
