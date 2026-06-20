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
2. [x] generate tests/td-fetch.lock (73 crate FODs + toolchain seed; network prep via
   gen-fetch-lock.scm).
3. [x] recipe-td-fetch.ts; td-fetch excluded from the guix-dependence census
   (self-host-specs) — a seed tool with no corpus oracle (its proof is the gate).
4. [x] rust-fetch gate (mk/gates/348): loopback round-trip + self-discrimination +
   oracle + repro. Runs green via `./check.sh rust-fetch`.
5. [x] verified-red evidence recorded; ready to land.

## Verified-red

- Census enrollment caught honestly: the FIRST `./check.sh rust-fetch` short-circuited at
  gate 070 (`guix repl: error: td-fetch: unknown package`) because recipe-td-fetch.ts
  auto-enrolled in the guix-dependence census; fixed by adding "td-fetch" to
  self-host-specs (the seed-tool exclusion). Census `.expected` UNCHANGED.
- SELF-DISCRIMINATION leg has teeth: disabled td-fetch's sha256 check (`if false && got
  != want`), rebuilt via the gate (the perturbed source re-interned → real rebuild, no
  vacuous cache hit), and the gate RED at exactly the right leg:
  `FAIL: td-fetch selftest ACCEPTED a wrong sha256 (000…000) — the verification is not
  load-bearing` (exit 1). The behavioral round-trip still passed (loopback body matches),
  so the leg that fired is specifically the verification control. Reverted to green
  (`git checkout fetch/src/main.rs`); the green gate re-runs PASS.
