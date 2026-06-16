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

## Next

- Drive `realize` for richer recipes (gettext) and register into td-store-db (not
  just a registration file) so td owns the store side of realize.
- A persistent daemon mode the loop invokes by default instead of guix-daemon.
- Resume offline-isolation / daemon-network (the parked work).
- Toolchain retired LAST (§5).
