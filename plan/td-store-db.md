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

## Increment 3 (DONE 2026-06-14): td READS its own store — "own the store, then diverge"

Direction (human 2026-06-14): "I'm worried we're so caught up with guix compatibility
that we won't be able to innovate … continue removing guile/guix dependencies." Chosen
pivot: finish td's store ownership the INDEPENDENCE way — td READS its own store (daemon
out of the query loop), NOT "make a daemon accept td's DB" (which would deepen the
compatibility cage). Once td writes+reads its own store, byte-identity to guix becomes
OPTIONAL and the format can diverge.

New `builder/src/store_db_read.rs` — a zero-dep SQLite *reader*, the inverse of
`store_db`: it parses the 100-byte file header, the `sqlite_master` schema b-tree (table
name → rootpage), and a table b-tree (leaf `0x0d`, descending interior `0x05`) into its
`(rowid, values)` rows, decoding records by the same serial-type/varint rules the writer
uses. New `td-builder store-query DB info|references` answers store queries off td's OWN
DB with that reader — NO sqlite3 engine, NO daemon in td's query path.

The `store-register` rung now reads td's DB THREE ways and asserts they agree: **td's own
reader == sqlite3 on the same bytes (the parser oracle) == the daemon's record (the
content oracle)** — `info` (every path's hash + narSize) and `references` (the full Refs
relation). td thus WRITES and READS its store DB itself; libsqlite/the daemon are
correctness oracles, not the format authority. The reader returns `Err` (never panics)
on a truncated/corrupt record — varints and value bodies are bounds-checked, overflow and
index/unknown page types are rejected — so td can read its OWN store defensively. Plus 5
new in-process unit tests (writer→reader round-trip: varints incl. boundary/​u64::MAX, a
ValidPaths-like table exercising serial types 3/4, all three store tables, bad-magic
rejection, truncated-input → Err not panic) — 35 cargo tests total. Additive: nothing in
the existing rung was removed or loosened (the sqlite3-vs-daemon differential stays); the
td-reader assertions are new strengthening.

## Increment 4 (DONE 2026-06-14): td PLACES a path into its own store — addToStore (write side)

The daemon's last *store-write* role on the simplest path: `addToStore`. New
`td-builder store-add-text NAME CONTENT-FILE STORE-DIR OUT-DB` — td computes the
addTextToStore path itself (`store::make_text_path`, already owned), WRITES the content
into a td-owned store dir as a canonical store file (a regular, read-only `0444` file),
and REGISTERS it in a td store DB (`store_db`). No daemon in td's write path.

New `store-add` rung (daemon = oracle, directive 4): the same bytes added via the
daemon's addTextToStore RPC (`store-add`, #27) — which writes the file to `/gnu/store`
and returns the path — give the IDENTICAL store path, and a store file BYTE-IDENTICAL
(by NAR hash) to the one td wrote; td's registration, read back with TD'S OWN reader
(`store-query`, #36), records that path + the NAR hash of what td wrote.

**Oracle choice (the WAL gotcha):** the first cut read the daemon's recorded `hash`/
`narSize` from the live DB (`?immutable=1`), but a *freshly* added path sits in the
daemon's WAL — invisible to an immutable `db.sqlite` snapshot (the store-register rung
only works because hello's closure is old/checkpointed). The daemon's **own store file**
is the WAL-free oracle and the *stronger* claim: td's store bytes == the daemon's store
bytes, compared by NAR hash. NAR ignores mtime + the read/write perm bits, so store
identity is metadata-independent. Boundary: td writes only its OWN scratch store/DB and
READS the daemon's store file; the daemon RPC adds a GC-able probe path (as the existing
store-add/drv-add rungs do) purely as the oracle.

td now owns the store loop on the flat path: WRITE the DB (#34/#35), READ the DB (#36),
and ADD a path with its bytes (this). The daemon is the oracle, not the authority.

## Increment 5 (DONE 2026-06-14): td computes GC reachability — the daemon's third role

The daemon's THIRD store role (after the DB authority and addToStore): **GC**. New
`td-builder store-closure DB ROOT` computes the GC-reachable closure of ROOT from a td
store DB — it reads the DB with td's OWN reader (`store_db_read::Db::closure`, #36) and
walks the `Refs` graph from ROOT (iterative DFS, rowid-set dedup for self-refs/cycles),
no daemon. This is GC's **mark/liveness** phase (what GC would KEEP); the destructive
sweep (deletion) is deliberately NOT done — boundary-safe, over td's OWN scratch DB.

New `store-gc` rung (daemon = oracle, directive 4): td WRITES hello's full-closure store
DB (`store-register`, #34/#35), then computes the reachable set from hello's output over
its OWN scanned `Refs` — and it equals `guix gc -R` (the daemon's own closure
computation) EXACTLY. Proves td's Refs graph + traversal reconstruct the daemon's GC
liveness set. The deriver scaffold row (id 2) has no in-edges, so it is correctly
unreached (matching `guix gc -R`, which excludes the `.drv`). +1 in-process unit test
(`closure_follows_the_refs_graph`: a 4-node graph with a self-ref + an unreachable node),
36 cargo tests.

td now owns the conceptual store loop: WRITE the DB (#34/#35), READ it (#36), ADD a path
(#38), and compute GC liveness (this) — all with the daemon as oracle, not authority.

## Later increments (sketch — not this PR)

- Recursive *directory* addToStore (canonical tree restore: recreate the tree with
  canonical metadata, the exec-bit honored) + references — the general add.
- The destructive GC SWEEP (delete the unreachable) into a td-owned store — the other
  half of GC, on a td store (never the host's).
- Eventually a td store backend the system can use, daemon retired for the build side.
- With write+read+add+GC-mark OWNED, deliberately DIVERGE the on-disk store format/schema
  where it buys something (the differential becomes a correctness check on td's chosen
  format, not a guix-compat constraint). Having a daemon accept td's DB is now OPTIONAL,
  not the goal.

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

**R3 the reader's parse is non-vacuous — unit level** (2026-06-14, increment 3).
Perturbed `store_db_read::read_varint` (`result << 7` → `result << 8`), ran `cargo
test`: `varint_roundtrip` failed (128 decoded as 256) and both table round-trips failed
("leaf cell payload overruns page" — the corrupted payload-length varint) ⇒ 3 reader
tests red while the writer/other tests stayed green. Proves the writer→reader round-trip
tests genuinely exercise td's parser, not a vacuous pass. Reverted; 34 green.

**R4 td's reader is load-bearing in the rung — integration level** (2026-06-14,
increment 3). Perturbed `store-query info` to read `narSize` from the wrong column
(`cols[3]` = the registrationTime sentinel = 1, instead of `cols[5]`), rebuilt (unit
tests untouched, so the binary still built), ran the rung: td's reader reported
`narSize=1` for every path while sqlite3 on the SAME bytes reported the true sizes
(1887496, 41145792, …) ⇒ the new `test "$td_read_info" = "$td_rows"` assertion fails ⇒
`store-register` red. Proves the td-reader assertions are non-vacuous and reader-specific
— they catch a wrong store-query answer the existing sqlite3-vs-daemon checks do not
(those stayed green). Reverted; rung green again.

**R5 td actually WRITES the real store bytes — store-add** (2026-06-14, increment 4).
Perturbed `store-add-text` to append a byte to the content it writes to disk
(`std::fs::write(&disk, [content.as_slice(), b"X"].concat())`), rebuilt, ran the
`store-add` rung: the file td wrote NAR-hashed to `sha256:2f298c…` ≠ the daemon's own
store file `sha256:eb40ea…` ⇒ the `test "$td_file_hash" = "$oracle_hash"` assertion fails
⇒ `store-add` red. Proves the byte-identity differential is load-bearing — td must place
store bytes identical to the daemon's, not just compute the right path. (The path
assertion stayed green: the path is computed from the original content, not the file.)
Reverted; rung green again.

**R6 the store-path differential is non-vacuous — store-add** (2026-06-14, increment 4).
Perturbed `store-add-text` to compute the path from the wrong name
(`make_text_path(&format!("{name}X"), …)`), rebuilt, ran the `store-add` rung: td
computed `…-td-store-add-probeX` ≠ the daemon's `…-td-store-add-probe` ⇒ the
`test "$td_path" = "$daemon_path"` assertion fails ⇒ `store-add` red. Proves the headline
"td computed the SAME store path as the daemon" assertion is non-vacuous. Reverted; rung
green again.

**R7 the closure traversal is load-bearing — unit level** (2026-06-14, increment 5).
Perturbed `Db::closure` to NOT follow edges (`stack.push(m)` → a no-op), ran `cargo
test`: `closure_follows_the_refs_graph` failed — `closure("/a")` returned `["/a"]` ≠
`["/a","/b","/c"]` ⇒ the reachability test red (the build gate runs the unit tests, so a
broken traversal fails the build). Proves the traversal logic is genuinely tested.
Reverted; 36 green.

**R8 the GC-closure differential is non-vacuous — store-gc** (2026-06-14, increment 5).
Perturbed the `store-closure` CLI arm to drop one path (`paths.into_iter().skip(1)`) —
the unit tests cover `Db::closure`, not the arm, so the binary still built — ran the
`store-gc` rung: td's closure (3 paths, bash-static dropped) ≠ `guix gc -R` (4 paths) ⇒
the `test "$td_reach" = "$oracle"` assertion fails ⇒ `store-gc` red. Proves the rung's
comparison against the daemon's `guix gc -R` is live and non-vacuous. Reverted; rung
green again.
