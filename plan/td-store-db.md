# plan/td-store-db.md — td owns the store database (begin replacing guix-daemon)

Track: **td-store-db** (DESIGN §7.1; the §2.2/§2.5 "replace a reused Guix component"
goal — guix-daemon. Human go-ahead 2026-06-14: "what's next" → "Replace the
guix-daemon"). Claim: claude-fable-4a2e33, 2026-06-14. Single writer. The daemon is the
ORACLE (prime directive 4: differential before replacement).

## The goal and the boundary

The guix-daemon's roles: (1) the store **SQLite DB** — `ValidPaths`/`Refs`/
`DerivationOutputs`, the record of what is valid in the store; (2) **build
coordination** + writing `/gnu/store`; (3) **GC**. td-builder already CONSTRUCTS (#22),
EXECUTES (#25), REGISTERS via the daemon RPC (#27), NAR-hashes, and computes store
paths — so build execution is td's. What is still ONLY the daemon's is the **store DB
authority**: deciding/writing the `ValidPaths` rows that make a path valid + queryable.

**Boundary (the host daemon is immutable infra — [[td-no-host-machine-changes]]):** we
do NOT stop the host daemon or write the real `/gnu/store` DB. td operates its OWN store
DB (a scratch/declared `GUIX_STATE_DIRECTORY`, the rootless-rung pattern), differential
against the host daemon's DB as oracle. Input resolution + the daemon building the
INPUTS stay Guix's, toolchain retired last (§5).

## Increment 1 (this track's first PR): `td-builder store-register`

td writes the store-DB **registration** for a built artifact itself — no daemon
`registerValidPaths`. The schema (read off the host DB): `ValidPaths(id, path, hash,
registrationTime, deriver, narSize)`, `Refs(referrer, reference)` (by id),
`DerivationOutputs(drv, id, path)`. td already computes every deterministic field:
- `hash` = `sha256:<base16>` — **byte-identical to what td-builder `nar-hash` emits**
  (verified: the daemon stores `sha256:0f28ab…`, base16, not base32);
- `narSize`, `deriver` (= the `.drv` path), `references` (the `scan` output).
`registrationTime` is the only non-deterministic field (the daemon sets "now") — excluded
from the differential.

Approach: td-builder emits the registration as **SQL** (`INSERT INTO ValidPaths … / Refs
… / DerivationOutputs …`), and the rung loads it into a fresh store DB via `sqlite3` —
the same SQLite engine the daemon uses (libsqlite); the AUTHORITY LOGIC (which rows,
with which values) is td's, in Rust. (Hand-rolling the SQLite file format is a possible
later zero-dep purism; sqlite3 is already a declared input, used by the `rootless` rung.)

**td writes the SQLite FILE FORMAT itself** — not SQL for the `sqlite3` engine. The
`store_db` module (zero-dep, unit-tested) writes the 100-byte file header, table
b-tree leaf pages (page type `0x0d`), and the record format (a header of serial-type
varints + the values), assembling a valid SQLite DB. This is the real replacement of
the daemon's libsqlite, not a thin SQL emitter.

Rung `store-register` (differential, daemon = oracle):
1. `td-builder store-register STORE-PATH DERIVER CANDIDATES-FILE OUT-DB` scans the
   artifact (NAR hash + size + refs, the `build` machinery) and WRITES a store DB at
   OUT-DB in pure Rust (its ValidPaths row fully computed; the references + deriver as
   minimal scaffolding rows so the joins resolve).
2. `sqlite3` confirms td's hand-written DB is a structurally valid SQLite file
   (`PRAGMA integrity_check` = ok) and reads back hello's row, refs, drv→output.
3. Assert those **match the daemon's** recorded registration (immutable read of the
   live DB): same `hash`, `narSize`, `deriver`, referenced PATHS, and the
   `DerivationOutputs(drv,"out",path)` mapping. Verify red: perturb a field ⇒ diverge.

This proves td reproduces the daemon's store-DB authority — writing the actual DB
bytes — for one artifact.

## Increment 2 (DONE 2026-06-14): full-closure registration

`store-register` now registers EVERY path in the artifact's closure (`guix gc -R`),
each fully scanned — no placeholder rows except the deriver (a `.drv`, not a closure
member). The differential asserts, byte-identical to the daemon: (1) every closure
path's `hash` + `narSize`, (2) the full inter-path `Refs` relation, (3) the artifact's
deriver + drv→output. Removes increment 1's scaffolding caveat. Per-path derivers of the
non-artifact members (the daemon's input-resolution) + `registrationTime` excluded.

## Later increments (sketch — not this PR)

- Have `guix`/a fresh daemon on td's `GUIX_STATE_DIRECTORY` report the artifact VALID
  (queryable end-to-end) — needs the exact daemon schema (indexes/trigger/sequence) so
  the daemon accepts td's hand-written DB.
- `addToStore` end-to-end in td (write the path + register) into a td store.
- GC reachability (the daemon's third role).
- Eventually a td store backend the system can use, daemon retired for the build side.

## Sub-task ladder

1. Claim + plan + DESIGN entry. — A.
2. `store_db` SQLite file-format writer (unit-tested) + `store-register` writing a store
   DB + the differential rung. Verify red. — B.
3. Full `./check.sh` green; PR. — C.

## Implementation progress

- **DONE 2026-06-14.** New `builder/src/store_db.rs` — a zero-dep SQLite FILE-FORMAT
  writer (the 100-byte header, table b-tree leaf pages `0x0d`, the record/serial-type
  varint encoding), 4 unit tests. `store-register` scans the artifact (NAR hash + size
  + refs, the `build` machinery) and WRITES a store DB in pure Rust. The `store-register`
  rung GREEN inside td's own sandbox: `sqlite3 PRAGMA integrity_check` = ok on td's
  hand-written DB, and hello's registration reads back BYTE-IDENTICAL to the daemon
  (hash `sha256:0f28ab…`, narSize 282616, deriver, all 3 referenced paths incl. the
  self-ref, the drv→output mapping). 30 cargo tests pass. Findings: the reference-scan
  candidates must be the output's runtime closure (`guix gc -R $out`), NOT the drv's
  build closure (`gc -R $drv` — those are drvs/sources, scan found zero refs); the
  references + deriver are written as minimal scaffolding rows so the joins resolve
  (full-closure rows + the exact daemon schema are later increments). NOTE: this replaced
  an earlier thin "emit SQL for sqlite3 to run" cut (human: "I don't see much code or
  tests") — td now writes the actual DB bytes.

## Verified-red log

**R1 the registration differential is non-vacuous** (2026-06-14). Perturbed
`store-register` to write the artifact's `narSize` as `size + 1` (`Value::Int(size + 1)`)
into td's store DB, rebuilt, ran the differential: `sqlite3` reads td's hand-written DB
row narSize as 282617 ≠ the daemon's 282616 ⇒ the rung's `test "$td_row" =
"$oracle_row"` fails ⇒ `store-register` red ("td's ValidPaths row … != the daemon's").
Proves the rung genuinely compares td's WRITTEN DB to the daemon's record (and that
sqlite3 really parsed td's bytes), not a vacuous pass. Reverted; rung green again.

**R2 the full-closure differential covers EVERY path** (2026-06-14, increment 2).
Perturbed the `others` loop to write a NON-artifact closure path's `narSize` as
`size + 1`, rebuilt, ran the per-path differential: all three non-artifact paths
diverged from the daemon (e.g. bash-static 1887497 ≠ 1887496, glibc 41145793 ≠
41145792) ⇒ the rung's `test "$td_rows" = "$oracle_rows"` fails ⇒ `store-register` red.
Proves the closure differential checks every path's registration, not just the
artifact's (which R1 covered). Reverted; rung green again.
