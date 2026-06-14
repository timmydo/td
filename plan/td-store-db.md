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

## Increment 6 (DONE 2026-06-14): recursive addToStore — td restores a directory TREE

The general write side (after the flat `store-add`, #38): td places a DIRECTORY TREE into
its own store. New `td-builder store-add-recursive NAME SRC STORE-DIR OUT-DB` computes the
content-addressed `source` path from the tree's recursive NAR sha256
(`make_store_path("source", …)` — the daemon's makeFixedOutputPath for recursive-sha256,
no references), CANONICALLY restores the tree with `copy_canonical` (structure + contents
+ the file EXECUTABLE bit + symlinks — the properties NAR captures; dir perms, the
read/write bits, and mtimes are NAR-irrelevant, so dirs are left writable for cleanup),
and registers it in a td store DB. No daemon in the write path. (The exec bit keys off
OWNER-exec `0o100` — exactly what td's own `nar.rs` serializer and the daemon's
canonicaliser (`S_IXUSR`) use, so a group/other-exec-only file stays non-executable;
a review-caught fidelity fix, with a `0o654` regression guard in the unit test.)

New `store-add-tree` rung (daemon = oracle, directive 4): the daemon's OWN interned
`td-builder` source tree (a real directory added via addToStore recursive, no refs —
lowered with `guix repl … (lower-object %builder-source)`, already in the store so no
fresh add / no WAL) gives the IDENTICAL content-addressed path, and a tree BYTE-IDENTICAL
(by NAR hash) to the one td restored; td's registration (read back by TD'S OWN reader)
records that path + the tree's NAR hash. +1 unit test
(`copy_canonical_is_nar_identical_with_exec_and_symlink`: a tree with an executable file
+ a symlink — the NAR-relevant cases the rung's source tree lacks), 37 cargo tests.
(Note: editing `builder/` changes the source's content-addressed hash, so the rung
recomputes the oracle path fresh each run — oracle and td always agree on current content.)

td now owns the full store WRITE side (flat #38 + recursive) plus read (#36), the DB
authority (#34/#35), and GC-mark (#39) — daemon as oracle throughout.

## Increment 7 (DONE 2026-06-14): td VERIFIES store integrity — guix gc --verify --check-contents

The daemon's store-integrity check. New `td-builder store-verify DB STORE-ROOT` reads the
recorded registration from a td store DB (`store_db_read`, #36) and re-NAR-hashes each
registered path at `STORE-ROOT/<basename>`, flagging (exit 1) any path whose content no
longer matches its recorded `hash` (corruption / disk-rot). No daemon. Composes the reader
(#36) + the NAR hasher — closing the loop write-hash (#34/#35) → read-hash (#36) → verify
content against it.

New `store-verify` rung, two legs: **(A) the daemon differential** — td first proves its
DB records the DAEMON's hashes for hello's closure (immutable live-DB read), then re-hashes
each path in the REAL `/gnu/store` and confirms it matches (exit 0): td independently
verifies the store content against the daemon's record, exactly `--check-contents`. **(B)
corruption detection** — a flat probe added to a td-owned store verifies OK, then a one-byte
corruption is DETECTED (verify exits nonzero, naming the path). Verified-red R11 (perturb
verify to never flag a mismatch ⇒ the corruption goes undetected ⇒ rung red). Boundary: td
READS `/gnu/store` + the td DB and writes only its own scratch store/DB/probe. (Note: store
files are `0444`, so injecting the corruption needs a `chmod u+w` first.)

td can now verify the integrity of a store against its registration — a prerequisite for
trusting a td-owned store backend.

## Increment 8 (DONE 2026-06-14): the destructive GC SWEEP — the other half of GC

After the mark/liveness `store-closure` (#39), the **sweep**. New `td-builder
store-gc-sweep STORE-DIR DB ROOT` computes the live set (closure of ROOT over the Refs),
DELETES every registered content path NOT reachable from ROOT from the td-owned STORE-DIR,
and rewrites the DB to the live set (ValidPaths + Refs renumbered). No daemon.

New `store-gc-sweep` rung (daemon = oracle, directive 4): a td-owned store is built by
copying hello's full closure (`cp -a`, then `chmod -R u+w` so it's deletable) and
registering it (`store-register`); after sweeping with ROOT=glibc (whose closure is a
PROPER subset — hello's closure is the chain hello→gcc-lib→glibc→bash-static), the
surviving store entries AND the rewritten DB hold EXACTLY `guix gc -R glibc` (the daemon's
own reachable set), and the dead paths' files are gone. Verified-red ×2: R12 (skip the file
deletion ⇒ dead files remain ⇒ survivors ≠ live) and R13 (skip the DB rewrite ⇒ the swept
DB still lists the dead). Boundary: the sweep deletes ONLY from the td-owned scratch
STORE-DIR (a `cp -a` copy) and rewrites only the scratch DB — the host `/gnu/store` is
NEVER touched.

td now owns BOTH halves of GC — mark (#39) and sweep — plus write (#34/#35), read (#36),
add (flat #38 + recursive #41), and integrity-verify (#43). Daemon as oracle throughout.

## Increment 9 (DONE 2026-06-14): addToStore WITH references

After the no-reference flat (#38) and recursive (#41) adds, the **with-references** case.
New `td-builder store-add-referenced NAME CONTENT-FILE REFS-FILE STORE-DIR OUT-DB` computes
the content-addressed path with the references FOLDED INTO THE TYPE (`make_text_path`:
`text:<sorted refs>` — the daemon's makeTextPath/makeType), WRITES the content into a
td-owned store (canonical 0444 file), and REGISTERS the path with its `Refs` to the
referenced paths (each a scaffolding ValidPaths row so the join resolves). No daemon.

The canonical referenced content-addressed item is a `.drv` (referenced by its input
drvs/srcs — a real, deterministic fixture; recursive `source` paths WITH references use the
same `makeType("source", refs)` rule but have no clean interned fixture, so the differential
runs on the `.drv`). New `store-add-referenced` rung (daemon = oracle, directive 4): for
hello's `.drv` and its references (`guix gc --references`), td computes the IDENTICAL store
path (the references folded in — drop one and it diverges), writes a `.drv` byte-identical
(by NAR hash) to the daemon's own, and registers EXACTLY the daemon's recorded references
(read back by td's own `store-query references`). Verified-red ×2: R14 (drop a ref from the
path computation ⇒ path diverges) and R15 (register one fewer ref ⇒ refs differ while the
path stays correct).

td now owns addToStore for flat (#38), recursive (#41), AND referenced paths — plus the DB
authority (#34/#35), read (#36), both halves of GC (#39/#47), and integrity-verify (#43).

## Later increments (sketch — not this PR)

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

**R9 the canonical tree restore preserves NAR-relevant properties — unit level**
(2026-06-14, increment 6). Perturbed `copy_canonical` to drop the executable bit (`mode =
0o444` for exec files too), ran `cargo test`: `copy_canonical_is_nar_identical_with_exec_
and_symlink` failed — the restored tree's NAR (`sha256:4754ae…`) ≠ the source's
(`sha256:acd008…`) ⇒ the test red (the build gate runs it). Proves the tree restore's
exec-bit fidelity — a property NAR captures that the rung's source tree lacks — is
genuinely tested. Reverted; 37 green.

**R10 the recursive content-addressed path differential is non-vacuous — store-add-tree**
(2026-06-14, increment 6). Perturbed `store-add-recursive` to compute the path from the
wrong type string (`make_store_path("sourceX", …)`) — `make_store_path`'s "source" type is
not unit-tested, so the binary still built — ran the `store-add-tree` rung: td computed
`…mxqj89…` ≠ the daemon's interned `…vwq9xz…` ⇒ the `test "$td_path" = "$src"` assertion
fails ⇒ `store-add-tree` red. Proves the rung's content-addressed path differential
against the daemon is live. Reverted; rung green again.

**R11 store-verify's corruption detection is load-bearing — store-verify** (2026-06-14,
increment 7). Perturbed `store-verify` to never flag a mismatch (`if false && &got !=
recorded`) — the arm is CLI glue, not unit-tested, so the binary still built — ran the
`store-verify` rung: the deliberately corrupted probe was NOT detected (verify exited 0) ⇒
the rung's `if store-verify …; then FAIL "did NOT detect the corrupted probe"` fires ⇒
`store-verify` red. Proves the integrity check actually compares re-hashed content to the
record, not a vacuous pass. Reverted; rung green again.

**R12 the sweep's DELETION is load-bearing — store-gc-sweep** (2026-06-14, increment 8).
Perturbed `store-gc-sweep` to skip the file deletion (`if false && entry.exists()`), ran
the rung: all 4 copied paths survived (`surv`: hello, gcc-lib, glibc, bash-static) ≠ the
2-path live set ⇒ the `test "$survivors" = "$live"` assertion fails ⇒ `store-gc-sweep` red.
Proves the destructive deletion actually happens. Reverted; rung green again.

**R13 the swept-DB rewrite is load-bearing — store-gc-sweep** (2026-06-14, increment 8).
Perturbed the sweep to skip rewriting the DB (left the original on disk), ran the rung: the
dead FILES were deleted (survivors == live passed) but the DB still listed all 4 paths ⇒
the `test "$db_paths" = "$live"` assertion fails ⇒ `store-gc-sweep` red. Proves the DB is
genuinely rewritten to the live set, independently of the file deletion. Reverted; rung
green again.

**R14 the references are load-bearing in the PATH — store-add-referenced** (2026-06-14,
increment 9). Perturbed `store-add-referenced` to compute the path from one fewer reference
(`make_text_path(name, &content, &refs[..refs.len()-1])`), ran the rung: td computed
`…0qmx7p2…-hello-2.12.2.drv` ≠ the daemon's `…zx4bn6w…` ⇒ the `test "$td_path" = "$drv"`
assertion fails ⇒ `store-add-referenced` red. Proves the references are folded into the
content-addressed path (makeTextPath's type). Reverted; rung green again.

**R15 the references are actually REGISTERED — store-add-referenced** (2026-06-14,
increment 9). Perturbed the registration loop to register one fewer reference
(`refs.iter().take(refs.len()-1)`) while leaving the path computation on the full set, ran
the rung: the path matched but td's registered references (8) ≠ the daemon's 9 ⇒ the
`test "$td_refs" = "$oracle_refs"` assertion fails ⇒ `store-add-referenced` red. Proves the
Refs registration is load-bearing, independently of the path. Reverted; rung green again.
