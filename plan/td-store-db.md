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

Rung `store-register` (differential, daemon = oracle):
1. Build hello via the daemon (oracle) → the real store path + the daemon's recorded
   `ValidPaths` row (hash/narSize/deriver) + its `Refs` (referenced paths).
2. `td-builder build` it in td's staged store → td's path/nar-hash/nar-size/refs/deriver.
3. `td-builder store-register …` emits the SQL; load it into a FRESH DB (schema-only).
4. Assert td's DB rows for the artifact **match the daemon's** on every deterministic
   field: same `hash`, `narSize`, `deriver`, and the same set of referenced PATHS
   (join `Refs`→`ValidPaths`), and the `DerivationOutputs(drv,"out",path)` mapping.
   Verify red: perturb a field (wrong hash / a dropped ref) ⇒ the differential diverges.

This proves td reproduces the daemon's store-DB registration authority for one artifact.

## Later increments (sketch — not this PR)

- Register the full closure into a td store DB and have `guix`/a fresh daemon on td's
  `GUIX_STATE_DIRECTORY` report the artifact VALID (queryable end-to-end), no daemon
  having written it.
- `addToStore` end-to-end in td (write the path + register) into a td store.
- GC reachability (the daemon's third role).
- Eventually a td store backend the system can use, daemon retired for the build side.

## Sub-task ladder

1. Claim + plan + DESIGN entry. — A.
2. `store-register` SQL emission + the differential rung. Verify red. — B.
3. Full `./check.sh` green; PR. — C.

## Implementation progress

- **DONE 2026-06-14.** `store-register` subcommand + the `store-register` rung GREEN
  inside td's own sandbox: for the corpus `hello`, td's emitted registration — loaded
  into a working copy of the store DB after deleting the daemon's own row — queries
  back BYTE-IDENTICAL to the daemon's record (hash `sha256:0f28ab…`, narSize 282616,
  deriver, all 3 referenced paths incl. the self-ref, the drv→output mapping). The
  candidates for the reference scan must be the output's runtime closure (`guix gc -R
  $out`), NOT the drv's build closure (`gc -R $drv` — those are drvs/sources, so the
  scan found zero refs). Reuses the `scan`/`nar` machinery `build` uses.

## Verified-red log

**R1 the registration differential is non-vacuous** (2026-06-14). Perturbed
`store-register` to emit `narSize` as `size + 1`, rebuilt, ran the differential: td's
row narSize (282617) ≠ the daemon's (282616) ⇒ the rung's `test "$td_row" =
"$oracle_row"` fails ⇒ `store-register` red ("td's ValidPaths row … != the daemon's").
Proves the rung genuinely compares td's written registration to the daemon's, not a
vacuous pass. Reverted; rung green again.
