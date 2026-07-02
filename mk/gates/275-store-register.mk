# store-register (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td both
# WRITES and READS the store SQLite DB for a TD-BUILT artifact's FULL CLOSURE itself — the
# daemon's `ValidPaths`/`Refs`/`DerivationOutputs` authority AND its store-query role, in
# pure Rust. `td-builder store-register` scans EVERY path in the closure (NAR hash + size +
# reference scan, the `build` machinery) and writes the SQLite FILE FORMAT directly (the
# `store_db` module: header + table b-tree leaf pages + the record/varint encoding, zero-dep)
# — the real replacement of the daemon's libsqlite, NO `sqlite3` engine writing it.
# `td-builder store-query` then READS that DB back with td's OWN pure-Rust SQLite reader
# (`store_db_read`) — NO sqlite3 engine and NO daemon in td's store-query path.
# R3 (guix-retirement ladder → #261): the SUBJECT is now td-BUILT (tests/store-subject.sh —
# hello via build-recipe, cache-hit off the build-recipes prelude) staged into a td-OWNED
# store, and its closure is CONTENT-SCANNED, so this gate runs with guix OFF PATH — no `guix
# build [-d]`, no `guix gc`, no /var/guix read. The removable DAEMON CONTENT ORACLE (the
# live /var/guix/db comparison of every path's hash/narSize, the full Refs relation and the
# drv->output) is DROPPED per CLAUDE.md directive 3 (called out in the PR). What remains is
# STRONGER-still self-consistency over a td-built subject: td writes a store DB that `sqlite3`
# confirms is structurally valid (`PRAGMA integrity_check` = ok — sqlite3 is a parser oracle,
# not guix), and whose registration — as answered by TD'S OWN READER — reads back
# BYTE-IDENTICAL to sqlite3 reading the same bytes for (1) every closure path's hash+narSize
# and (2) the full inter-path Refs relation; and a deriver that IS itself a closure member is
# registered ONCE (no duplicate ValidPaths row). Boundary: td writes only its OWN scratch
# store/DB; the host store is untouched. Needs td-builder + the corpus build → heavy pool +
# the build-recipes prelude.
HEAVY_GATES += store-register
BUILD_GATES += store-register
store-register:
	@echo ">> store-register: td WRITES the store SQLite DB for a TD-BUILT hello's FULL CLOSURE (pure-Rust file format) and READS it back byte-identically to sqlite3 (guix off PATH; no guix build, no guix gc, no /var/guix read)"
	@set -euo pipefail; \
	. tests/store-subject.sh; \
	scratch="$(CURDIR)/.store-register-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	td_store_subject "$$scratch" || exit 1; \
	n="$$SUBJ_N"; \
	echo ">> td WRITES the store SQLite DB for the $$n-path closure at $$scratch/td.db (no sqlite3 engine — td emits the SQLite bytes)"; \
	"$$TB" store-register "$$SUBJ_ROOT" "$$SUBJ_DRV" "$$SUBJ_CLOSURE" "$$scratch/td.db"; \
	test -s "$$scratch/td.db" || { echo "FAIL: td wrote no store DB" >&2; exit 1; }; \
	echo ">> sqlite3 validates td's hand-written DB: $$(sqlite3 "$$scratch/td.db" "PRAGMA integrity_check")"; \
	test "`sqlite3 "$$scratch/td.db" "PRAGMA integrity_check"`" = "ok" || { echo "FAIL: td's store DB is not a valid SQLite file (integrity_check failed)" >&2; exit 1; }; \
	rowsql="SELECT path||'|'||hash||'|'||narSize FROM ValidPaths WHERE hash IS NOT NULL ORDER BY path"; \
	refsql="SELECT a.path||'|'||b.path FROM Refs r JOIN ValidPaths a ON r.referrer=a.id JOIN ValidPaths b ON r.reference=b.id"; \
	td_rows=`sqlite3 "$$scratch/td.db" "$$rowsql"`; \
	test "`echo "$$td_rows" | wc -l`" = "$$n" || { echo "FAIL: td registered $$(echo "$$td_rows" | wc -l) paths, expected $$n" >&2; exit 1; }; \
	regpaths=`echo "$$td_rows" | cut -d'|' -f1`; \
	test "$$regpaths" = "`sort -u "$$SUBJ_CLOSURE"`" || { echo "FAIL: the registered path set != the staged closure" >&2; exit 1; }; \
	echo "   td registered all $$n closure paths (hash + narSize), exactly the staged closure"; \
	echo ">> td READS its own store DB itself (td-builder store-query — a pure-Rust SQLite reader; NO sqlite3 engine, NO daemon in td's query path):"; \
	td_read_info=`"$$TB" store-query "$$scratch/td.db" info`; \
	test "$$td_read_info" = "$$td_rows" || { echo "FAIL: td's reader disagrees with sqlite3 reading the SAME td.db bytes (info)" >&2; echo "$$td_read_info" | sed 's/^/  td-read: /' >&2; echo "$$td_rows" | sed 's/^/  sqlite3: /' >&2; exit 1; }; \
	echo "   info: td's reader == sqlite3 (same bytes) for all $$n paths' path|hash|narSize"; \
	td_refs=`sqlite3 "$$scratch/td.db" "$$refsql ORDER BY 1"`; \
	td_read_refs=`"$$TB" store-query "$$scratch/td.db" references`; \
	test "$$td_read_refs" = "$$td_refs" || { echo "FAIL: td's reader disagrees with sqlite3 reading the SAME td.db bytes (references)" >&2; exit 1; }; \
	echo "   references: td's reader == sqlite3 ($$(echo "$$td_read_refs" | wc -l) edges of the inter-path Refs relation)"; \
	echo ">> deriver-in-closure: a DERIVER that is itself a closure member is registered ONCE — no duplicate ValidPaths row"; \
	fakedrv=`grep -vxF "$$SUBJ_ROOT" "$$SUBJ_CLOSURE" | head -1 || true`; \
	test -n "$$fakedrv" || { echo "FAIL: closure has no member other than the artifact to use as an in-closure deriver" >&2; exit 1; }; \
	"$$TB" store-register "$$SUBJ_ROOT" "$$fakedrv" "$$SUBJ_CLOSURE" "$$scratch/td-dic.db"; \
	test "`sqlite3 "$$scratch/td-dic.db" "PRAGMA integrity_check"`" = "ok" || { echo "FAIL: the deriver-in-closure DB is not valid SQLite" >&2; exit 1; }; \
	dic_total=`sqlite3 "$$scratch/td-dic.db" "SELECT COUNT(*) FROM ValidPaths"`; \
	dic_distinct=`sqlite3 "$$scratch/td-dic.db" "SELECT COUNT(DISTINCT path) FROM ValidPaths"`; \
	test "$$dic_total" = "$$n" -a "$$dic_distinct" = "$$n" || { echo "FAIL: deriver-in-closure produced $$dic_total rows ($$dic_distinct distinct), expected $$n with no duplicate — the closure-member deriver was registered twice" >&2; sqlite3 "$$scratch/td-dic.db" "SELECT path,COUNT(*) c FROM ValidPaths GROUP BY path HAVING c>1" >&2; exit 1; }; \
	echo "   the closure-member deriver is registered once ($$n rows, no duplicate)"; \
	rm -rf "$$scratch"; \
	echo "PASS: td WROTE the store SQLite DB for a TD-BUILT hello's full $$n-path closure itself in pure Rust AND READ it back itself (td-builder store-query — a pure-Rust SQLite reader, no sqlite3 engine and no daemon in td's own store-query path). The subject is td-built and its closure content-scanned (guix off PATH; no guix build, no guix gc, no /var/guix read). sqlite3 PRAGMA integrity_check = ok on td's bytes; every path's hash + narSize and the full inter-path Refs relation, as answered by TD'S OWN READER, are BYTE-IDENTICAL to sqlite3 reading the same bytes (the parser oracle); and a closure-member deriver is registered once. The removable daemon CONTENT oracle (the /var/guix/db comparison) was dropped (directive 3). registrationTime + the non-artifact per-path derivers are excluded; the exact daemon schema (indexes/trigger) is a later increment."
