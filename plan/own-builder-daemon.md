# plan/own-builder-daemon.md — stand up td's OWN builder daemon (move-off-Guile §5)

Track: **own-builder-daemon**. Goal: the loop realizes derivations with td, not
guix-daemon. The guts exist (td-builder executes drvs [td-drv-build], td-store-db
owns the SQLite store, rootless builds NAR-equal); this track wires them into the
realize path and, eventually, a daemon the loop uses by default — the home of the
parked **offline-isolation** daemon-network work (rescoped to "the own-builder era").
Handle: claude-fable-2715d4.

## Increment 1 (PR #69 — td-realize): realize without guix-daemon

`td-builder realize DRV STORE-DB SCRATCH` (builder/src/main.rs): parse DRV → resolve
input ROOTS (input-srcs + each input drv's output paths, read from that .drv) →
compute the closure ITSELF via `store_db_read::Db::closure` over STORE-DB's Refs graph
(the `guix gc -R` the daemon did, now td's reader) → build in the userns sandbox →
register (shared `build_and_register`, extracted from `build`). Reading guix's live
`/var/guix/db/db.sqlite` with td's OWN reader is "own, then diverge" (shared store,
td's reader, no daemon process). guix-daemon is no longer in the realize path — only
the differential oracle.

Gate `td-realize` (355), subject = td-build hello drv:
- DURABLE: td computed the closure itself (non-empty); the realized hello runs.
- MIGRATION ORACLE (removable when guix retires): output path/NAR/size/deriver ==
  the daemon's build of the same drv.

Verified-red (closure step is load-bearing): (A) `realize` against a bogus store-db
errors ("not a SQLite 3 database") — it genuinely reads the db, not a no-op; (B)
`build` with an EMPTY closure fails to even spawn the builder ("No such file or
directory") — the userns sandbox RESTRICTS to the staged closure, so computing it
correctly matters. Both confirm the realize path is not vacuous.

## Increment 2 (PR #70 — td-offline): offline-isolation for td's builder

The parked **offline-isolation** work (rescoped 2026-06-11 to "the own-builder era")
resumed. `offline` (185) proves a non-fixed-output build can't reach the network under
GUIX-DAEMON; `td-offline` (360) proves td's OWN builder does the same. `td-builder
realize` runs the existing DRV_SANDBOX probe (tests/offline-drv.scm) in td's
userns+NEWNET sandbox — realize SUCCEEDING ⇒ the build saw only `lo` and egress failed
(DURABLE, the probe asserts it; no guix oracle). Discrimination control: a
userns+netns given a dummy non-lo interface, where the same /proc/net/dev check DOES
see it (lo,dummy0) — so the isolation assertion is load-bearing, not vacuous (the probe
would red the build if td's builder ever leaked an interface). All-durable.

Note: inside the offline loop the OUTER loop-sandbox netns is already loopback-only, so
this gate guards td's builder END-TO-END (it produces lo-only builds) + that the check
discriminates; a fully differential "td's NEWNET strips a parent interface" proof waits
for a network-present daemon harness.

## Increment 3 (PR #71 — td-realize-store): realize a real recipe + own the store record

`realize` is now a COMPLETE daemon op — build AND own the store record — on a
REAL-dependency recipe. builder/src/main.rs: `build_and_register` returns per-output
records; `realize` writes a td store-db (new `write_output_db`, store_db / pure Rust)
registering the built output (the daemon's post-build registration, no daemon).
Subject = gettext-minimal (inputs libunistring/libxml2/ncurses + flags + makeFlag +
two phases — a real dep graph, not just the toolchain). Gate `td-realize-store` (365):
DURABLE behavioral — the realized gettext-tools run (msgfmt + xgettext); DURABLE
structural — store-query reads the output record back from td's db (write → read
round-trip); MIGRATION ORACLE — that record (path|hash|narSize) equals the daemon's
ValidPaths row.

Verified-red / load-bearing: the round-trip reds on an empty/missing db (store-query
finds no record), and the daemon differential is a value comparison that reds on a
wrong hash/size — both new assertions are non-vacuous.

## Next

- A persistent daemon mode the loop invokes by default instead of guix-daemon.
- A network-present daemon harness → fully differential offline-isolation.
- Toolchain retired LAST (§5).
