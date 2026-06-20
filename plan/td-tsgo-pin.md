# td-tsgo-pin — consumer swap: tsgo off guix-as-fetcher (move-off-Guile §5)

Handle: claude-fable-5caf33 · started 2026-06-20 · section: side · stacks on td-fetch (#116)

## Goal

Retire the 15 in-loop `guix build -e '(@ (system td-ts) td-tsgo-tarball)'` invocations.
The loop is offline (only the host daemon has network), so the warm is a host PREP:
**td-fetch** fetches+verifies the tsgo tarball, the daemon `add-to-store`s the *verified
bytes* (NOT a guix url-fetch) — landing at the SAME FOD path the origin produces (proven:
`add-to-store(name,sha256-flat) == /gnu/store/iy52hn6…-typescript-linux-x64-7.0.1-rc.tgz`).
The loop reads a committed pin; no guix-as-fetcher in the gates. The daemon is only the
store (own-then-diverge; retired last). The guix origin stays as the pin/oracle.

## Design

- **tests/td-tsgo.lock** — committed pin: url + sha256 + the FOD store path.
- **tools/warm-tsgo.sh** — host PREP (network): if pin path warm → no-op; else build
  td-fetch (host `cargo build`, breaks the tsgo↔td-fetch bootstrap circularity — no tsgo
  needed), `td-fetch fetch url sha tmp`, `add-to-store` the verified file → assert ==
  pin path. The daemon stores; td-fetch fetched.
- **tests/tsgo.sh** — rewritten: read the pin path (no guix arg), assert warm, extract.
- **15 sites** (195-ts, 205, 220, 222, 224, 330, 335, 340, 345, 348, 350, 352,
  365-bootstrap, 365-build-plan, Makefile prelude): `tgz=$(guix build -e …); tsgo=$(tsgo.sh "$tgz")`
  → `tsgo=$(tsgo.sh)`.
- **check.sh** — run `tools/warm-tsgo.sh` in the host prelude (idempotent; near-instant
  when warm) so `./check.sh` stays self-sufficient.
- **ci/build-ci-image.sh** — keep the tsgo FOD an export root so check-fast (the `ts`
  FAST_GATE) finds it offline in the fast-image.
- **pin↔origin oracle** — a check asserts tests/td-tsgo.lock matches the origin in
  system/td-ts.scm (bumping one without the other reds).

## Sub-task ladder

1. [x] tests/td-tsgo.lock + tools/warm-tsgo.sh; add-to-store lands at the FOD path
   (proven: `add-to-store(name,sha256-flat) == /gnu/store/iy52hn6…tgz` == the origin FOD).
2. [x] rewrite tests/tsgo.sh (pin-based); swap the 15 sites (14 uniform + 348 compiler).
3. [x] check.sh host-prelude warm; `./check.sh ts` + `tsgo-pin` green, NO `guix build -e
   td-tsgo-tarball` in the gates.
4. [x] CI: ci/lower-fast-drvs.sh lowers td-tsgo-tarball (the pin FOD) instead of the stale
   node/td-typescript → a rebuilt fast-image carries the path; warm-tsgo.sh daemon-FOD
   fallback keeps the cargo-less runner green until then. **Human step: `PUSH=1
   TD_TIER=fast ci/build-ci-image.sh` to bake the path into the deployed image.**
5. [~] affected-checks mapped; verified-red recorded; full ./check.sh running; then land.

## Verified-red

- tsgo-pin content check has teeth: corrupting `tests/td-tsgo.lock` sha256
  (`4689c7e8…` → `0000dead…`) reds tsgo-pin at `FAIL: warm tarball sha256 … != pin …`
  (exit 1). Reverted to green. So the pin can't silently point at the wrong content.
- Bootstrap-circularity note: building td-fetch (rust-fetch gate) needs tsgo, but warming
  tsgo needs td-fetch — broken by warm-tsgo.sh's HOST `cargo build` (no tsgo), with the
  hermetic stage0 build proven separately by the rust-fetch gate.
