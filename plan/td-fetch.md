# td-fetch — td's own seed fetcher (move-off-Guile §5)

Handle: claude-fable-5caf33 · started 2026-06-20 · section: side

## Goal

td OWNS fetch+verify of its pinned fixed-output seeds. Today every pinned seed (the
tsgo tarball, the `.crate` deps, source tarballs) is fetched by **guix url-fetch** — the
host guix-daemon performs the fixed-output download with network (the in-loop processes
are loopback-only; only the daemon has network — see check.sh §56–62). This track builds
td's OWN fetcher — a small vendored-Rust HTTP(S) client that GETs a blob and verifies its
sha256 against the pin — and proves it on the **real tsgo tarball**, byte-identical to
guix's `td-tsgo-tarball` origin.

This is the *capability + oracle* brick (the rhythm of ts-eval Brick 4 → 4b/4c): swapping
the 13 in-loop consumers off `guix build -e '(@ (system td-ts) td-tsgo-tarball)'` onto a
td-warmed path is the follow-on, since the cold external warm is a host-network PREP
(can't run inside the offline loop).

## Design

- **fetch/** — the `td-fetch` crate. `ureq` (rustls/ring TLS, pure-Rust — no
  node/curl/openssl) + `sha2`. Two modes:
  - `td-fetch fetch URL SHA256-HEX OUT` — the real fetcher: GET (http/https), verify
    sha256, write OUT. The external TLS fetch runs in the network PREP on the host (§5
    "warm store in"); the offline loop never egresses.
  - `td-fetch selftest FILE SHA256-HEX` — self-contained LOOPBACK round-trip: serve
    FILE's bytes over HTTP on `127.0.0.1:<ephemeral>` from a worker thread, fetch+verify
    them back through the SAME client path (offline, like the russh gate's loopback SSH).
    The loopback server is std::net only — adds no crate to the closure.
- **tests/td-fetch.lock** — toolchain seed + the 74 `*.crate` FOD store paths
  (static.crates.io, Cargo.lock-pinned), same shape as td-ts-eval.lock / td-russh-demo.lock.
- **tests/ts/recipe-td-fetch.ts** — `buildSystem: "rust"`, `bins: ["td-fetch"]`, source key
  `td-fetch-source`.
- **mk/gates/<NNN>-rust-fetch.mk** (BUILD_GATE, after build-recipes), modeled on rust-russh:
  - [DURABLE structural] the .drv builder is the stage0 path; the .drv carries
    TD_VENDOR_CRATES (74).
  - [DURABLE behavioral] the td-built td-fetch round-trips the REAL tsgo tarball over
    loopback HTTP and verifies its sha256 — fetch+verify works end to end.
  - [SELF-DISCRIMINATION] a wrong sha256 reds it — the verification is load-bearing.
  - [MIGRATION ORACLE, removable] td-fetch's verified sha256 == guix's `td-tsgo-tarball`
    origin pin (`guix hash` base32 == the pin in system/td-ts.scm).
  - [DURABLE repro] td-builder check double-build agrees the td-fetch build is reproducible.

## Confirmed (host, before vendoring)

- td-fetch builds (74 deps, ureq+rustls/ring+sha2, 16s).
- `selftest` correct sha → exit 0 (loopback round-trip OK); wrong sha → exit 1 (mismatch).
- `fetch` over real HTTPS/TLS of the tsgo tarball → 10351486 bytes, sha256
  `4689c7e8…167d54`; `guix hash` of those bytes == the pinned base32
  `0m3x2q991qngixkmxnp4fr6d55ia4h30046x0sl85vpvs3lcg2a6` in system/td-ts.scm — the
  source-level oracle holds.

## Sub-task ladder

1. [x] claim track; td-fetch crate (fetch/) builds; modes confirmed host-side.
2. [ ] generate tests/td-fetch.lock (74 crate FODs + toolchain seed; network prep).
3. [ ] recipe-td-fetch.ts; build td-fetch via build-recipe + stage0 (guix/Guile off PATH).
4. [ ] rust-fetch gate: loopback round-trip + self-discrimination + oracle + repro.
5. [ ] verified-red evidence; land.

## Verified-red

(to record as each assertion is broken and seen red)
