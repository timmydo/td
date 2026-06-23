# td-feed — working notes

Handle: claude-fable-65585b · claimed 2026-06-23

## Goal

A local pure-Rust webserver, `td-feed` (crate `feed/`, sibling of `fetch/`), that:
1. holds an **index** of every artifact this repo downloads over the network
   (td-fetch seed blobs + url-fetch source tarballs + all ~1830 static.crates.io
   `.crate` deps), each pinned `<path> <url> <sha256>`;
2. **warms** its persistent store from the index during the network-permitted host
   PREP (egress allowed, fetch+verify each artifact);
3. **serves** the offline loop as a URL-path mirror (`GET /<host>/<path>`,
   verify-on-serve against the indexed sha256) over loopback — no in-loop egress.

Then reroute td-NATIVE fetchers (td-fetch, tools/warm-*.sh) through the feed via a
`TD_FEED_BASE` URL rewrite. The guix daemon keeps its own fetching (retired last §5).

## Decisions (human, 2026-06-23)

- Index: **everything content-addressed** (seeds + tarballs + all crates).
- Serve model: **URL-path mirror/proxy** (preserve `<host>/<path>`), verify against
  the index sha256.
- Reroute: **td-native fetchers only** — not the guix daemon.
- Run model: **host PREP, then serve offline** — keeps hermeticity directive 2.

## Brick ladder

- [ ] **B1 — the `td-feed` binary + loopback selftest.** `feed/` crate, sibling of
  `fetch/` (ureq + sha2, std::net server). Subcommands: `warm INDEX STORE`,
  `serve STORE INDEX --addr`, `selftest`. The selftest stands up a loopback ORIGIN
  server, warms a tiny index from it, serves the store on a 2nd loopback port, fetches
  through the feed + verifies — and reds on a perturbed artifact. Host `cargo build`.
  Verified-red: perturb the stored byte / the index hash → selftest reds.
- [ ] **B2 — index format + generator.** `tools/gen-feed-index.sh` emits
  `tests/td-feed.index` from the locks (`^url`/`^sha256`), the url-fetch origins, and
  the crate closures. Committed/pinned like a lock. A `feed-index` self-consistency
  check (every line parses; no dup paths; sha256 is 64-hex).
- [ ] **B3 — reroute.** `TD_FEED_BASE` URL rewrite in td-fetch (and `warm-*.sh`):
  `https://HOST/PATH` -> `$TD_FEED_BASE/HOST/PATH`, sha256 unchanged. Demo: the tsgo
  warm path served through the feed.
- [ ] **B4 — from-source gate.** `mk/gates/<NNN>-td-feed.mk`: build td-feed from source
  via `td-builder build-recipe` (rust-fetch template), run the offline loopback
  round-trip + verified-red + index self-consistency, and `td-builder check` repro.

## Verified-red evidence

(record per brick as completed)
