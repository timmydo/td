# plan/rootless-snapshot-race.md — race-free rootless store-DB snapshot

Track: **rootless-snapshot-race** (claude-opus-117569, 2026-06-15). Single writer.
Follows the honesty-fixes PR (#51), which documented the residual but deferred the fix.

## Problem

The `rootless` gate stages a snapshot of the host store DB so its unprivileged nested
daemon knows which paths are valid (`tests/rootless.sh`: `cp /var/guix/db/db.sqlite`
+ wal → checkpoint → integrity_check). That copy is race-free WITHIN one check (the
gate runs last/alone) but NOT against a SECOND concurrent check (DESIGN §7.3 permits
two): the other check's heavy build gates drive the shared host daemon, which writes
the store DB while we copy it → a torn copy → `integrity_check` fails loud. Flaky, not
silently wrong — but a real flaky failure under concurrent checks.

## Why the obvious fixes are blocked (non-root constraint)

- **Hold the daemon's lock during the copy.** `/var/guix/db/big-lock` and
  `/var/guix/gc.lock` are `0600 root` — a non-root client cannot `flock` them.
- **sqlite online backup (`.backup`/`VACUUM INTO`) of the live DB.** Reading the live
  WAL DB needs an `-shm` wal-index (re)created in the root-owned `/var/guix/db` →
  EPERM (the original R8 finding). So no consistent live read as a non-root client.

Both "proper" mechanisms dead-end on permissions.

## Direction (human-confirmed 2026-06-15): build the DB from the closure

Don't read the live DB at all. CONSTRUCT the snapshot DB from the STATIC closure the
gate already computes (`$scratch/paths.txt` = `guix gc -R` of the inputs + `img_out` +
the guix/daemon packages + GUIX_ENVIRONMENT). A DB built from a fixed path list +
per-path content hashes has nothing for a concurrent bulk-writer to tear. Advances
"td owns the store DB" (the td-store-db arc).

**The tool already exists.** `td-builder store-register STORE-PATH DERIVER
CANDIDATES-FILE OUT-DB` (builder/src/main.rs) registers a whole closure: it scans each
path (real NAR hash + size + refs among the closure) and writes `ValidPaths`/`Refs`/
`DerivationOutputs` as the SQLite file format in pure Rust — reading only the closure
list + the path contents, never the live DB. td's computed hash == the daemon's
recorded hash (proven byte-identical by the td-store-db differential), so `img_out`'s
recorded hash stays a valid `--check` oracle.

## Open risks (what the spike must answer)

1. **Daemon acceptance.** Unproven that a real *unprivileged guix-daemon* reads td's
   hand-built DB and treats the paths as valid. td-store-db proved td writes
   byte-identical rows and serves via *its own* reader — a live daemon reading td's DB
   file is the new unknown (schema version file = "1"; the daemon should add its own
   `CREATE INDEX IF NOT EXISTS` on open since the scratch DB is writable; `Signatures`/
   `ultimate` columns are absent from td's tables — need to confirm the daemon tolerates
   that).
2. **Deriver-in-closure duplicate.** `store-register` assumes the DERIVER `.drv` is NOT
   a closure member (it writes a path-only "scaffolding" row for it at id 2). But the
   rootless `img_drv` IS in `paths.txt` (added to the `gc -R` set so the daemon can read
   the .drv) → the `others` loop would also write a full row for `img_drv`, a DUPLICATE
   `ValidPaths` row. Must dedupe (small store-register change) or feed CANDIDATES without
   the drv and register the drv separately.

## Increment ladder

1. **Spike (de-risk daemon acceptance).** Build a td `store-register` DB for a SMALL
   closure with a SEPARATE deriver (e.g. `hello`); stand up an unprivileged guix-daemon
   on it (the rootless.sh machinery) and assert it sees the path valid
   (`guix gc --references`). Smallest proof the daemon eats a td-built DB.
2. **Deriver-in-closure.** Handle `img_drv` ∈ closure (dedupe in store-register, with a
   differential vs the daemon proving the rows still match).
3. **Swap.** Replace rootless.sh's `cp`-snapshot with `td-builder store-register` over
   `paths.txt`; the `rootless` gate green against the constructed DB. Verified-red:
   perturb the closure (drop a path / corrupt a hash) ⇒ validity guard or `--check` reds.
4. **Seal.** Remove the live-DB `cp` entirely; structural assertion that rootless never
   reads `/var/guix/db` (race eliminated by construction, not mitigated).

## Status

Claimed + designed 2026-06-15. Spike next.
