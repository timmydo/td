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

### B1 (2026-06-23, commit 2d5efb5)
The `td-feed selftest` loopback round-trip, perturbed three ways (Edit + rebuild, then
`git checkout` to restore green):
- verify-on-serve disabled (`if false && hex != want`) → `feed SERVED a corrupted store
  artifact — verify-on-serve is not load-bearing`, exit 1.
- `warm` accepts any hash (`if false && got != sha256`) → `warm ACCEPTED a wrong sha256 —
  verification is not load-bearing`, exit 1.
- serve truncated body (`&bytes[1..]`) → `feed-served bytes differ from the origin
  artifact`, exit 1.
Restored → selftest green, exit 0.

### B2 (2026-06-23, commit e66fde8)
`tools/gen-feed-index.sh` → `tests/td-feed.index`: 1838 crates + 1 seed blob. Validated
self-consistent (all 3-field, all 64-hex sha256, no dup paths) and TRUTHFUL — a join over
all realized crates: 1838 index == 1838 realized, **zero** sha mismatches, zero missing.

### B3 (reroute, host demo)
Built td-fetch (host) with `TD_FEED_BASE` and td-feed; pre-warmed a store, served it, then
`TD_FEED_BASE=http://127.0.0.1:PORT td-fetch fetch https://origin.invalid/blob <sha> out`:
- td-fetch rewrote the URL to `…/origin.invalid/blob`, fetched THROUGH the feed, verified,
  wrote out (stdout: `(via feed …)`); out sha256 == pin. **Reroute works.**
- a wrong sha via the feed → `sha256 mismatch`, exit 1 (verification still load-bearing).

### B4 (gate index assertions, isolated verified-red)
The gate's `tests/td-feed.index` checks, run against perturbed copies:
- 2-field line → `RED: non-3-field`; non-64-hex sha → `RED: bad sha`; duplicate path →
  `RED: dup paths`; a perturbed index sha for a vendored crate ≠ realized content →
  truthfulness check reds. Green index passes all. (The from-source build + selftest +
  repro legs are validated by the `./check.sh td-feed` run.)

## Brick status
- B1 binary — DONE (commit 2d5efb5).
- B2 index + generator — DONE (commit e66fde8).
- B3 reroute (TD_FEED_BASE in td-fetch) — DONE, host-demonstrated.
- B4 from-source gate (mk/gates/350-td-feed.mk) — DONE, **`./check.sh td-feed` GREEN**.
  Builds td-feed from source via build-recipe (stage0 builder, guix/Guile off PATH);
  durable legs all pass: behavioral selftest (warm→serve→fetch loopback) + self-discrim,
  index self-consistent (1839 lines) + truthful (73/73 vendored crate sha256 == content),
  td-builder check double-build reproducible. (Two bugs found+fixed by the gate run:
  feed/Cargo.lock had drifted to log 0.4.33 vs the vendored 0.4.32 → re-seeded the lock
  from fetch/Cargo.lock; and the index checks used awk, absent here → rewrote awk-free.)

## Follow-on (noted, not this PR)
- Wire check.sh's host prelude to `td-feed warm` the index into a persistent store + start
  `td-feed serve` on loopback + export TD_FEED_BASE for the whole loop (spine change,
  exclusive landing). This PR delivers the CAPABILITY ("can be served through the feed");
  the full forced cutover of every loop fetcher is the follow-on.
- `tools/warm-feed.sh` PREP helper (build td-feed + warm the full index) — follow-on.
