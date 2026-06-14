# store-register (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td
# both WRITES and READS the store SQLite DB for an artifact's FULL CLOSURE itself — the
# daemon's `ValidPaths`/`Refs`/`DerivationOutputs` authority AND its store-query role,
# in pure Rust. `td-builder store-register` scans EVERY path in `guix gc -R hello` (NAR
# hash + size + reference scan, the `build` machinery) and writes the SQLite FILE FORMAT
# directly (the `store_db` module: header + table b-tree leaf pages + the record/varint
# encoding, zero-dep) — the real replacement of the daemon's libsqlite, NO `sqlite3`
# engine writing it. `td-builder store-query` then READS that DB back with td's OWN
# pure-Rust SQLite reader (`store_db_read`) — NO sqlite3 engine and NO daemon in td's
# store-query path. The differential (daemon = oracle, prime directive 4): td writes a
# store DB that `sqlite3` confirms is structurally valid (`PRAGMA integrity_check` = ok),
# and whose registration — as answered by TD'S OWN READER — reads back BYTE-IDENTICAL
# both to sqlite3 reading the same bytes (the parser oracle) and to the daemon's record
# (the content oracle): (1) EVERY closure path's hash + narSize, (2) the FULL inter-path
# Refs relation (referrer→reference), and (3) the artifact's deriver + drv→output.
# `registrationTime` (the daemon's "now") and the per-path derivers of the non-artifact
# closure members (the daemon's input-resolution) are excluded; only the deriver `.drv`
# is a scaffolding row (it is not a closure member). Boundary: the host DB is read
# IMMUTABLY only; td writes only its OWN scratch DB — the host daemon stays immutable
# infra. Needs td-builder built, so it slots in the heavy pool.
HEAVY_GATES += store-register
store-register:
	@echo ">> store-register: td WRITES the store SQLite DB for hello's FULL CLOSURE (pure-Rust file format) — every path's registration reads back byte-identical to the daemon"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	out=`guix build hello`; drv=`guix build -d hello`; \
	test -n "$$out" -a -n "$$drv" || { echo "ERROR: could not realise hello" >&2; exit 1; }; \
	scratch="$(CURDIR)/.store-register-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	guix gc -R "$$out" | sort -u > "$$scratch/closure.txt"; \
	n=`wc -l < "$$scratch/closure.txt"`; \
	echo ">> td WRITES the store SQLite DB for the $$n-path closure at $$scratch/td.db (no sqlite3 engine — td emits the SQLite bytes)"; \
	"$$tb" store-register "$$out" "$$drv" "$$scratch/closure.txt" "$$scratch/td.db"; \
	test -s "$$scratch/td.db" || { echo "FAIL: td wrote no store DB" >&2; exit 1; }; \
	echo ">> sqlite3 validates td's hand-written DB: $$(sqlite3 "$$scratch/td.db" "PRAGMA integrity_check")"; \
	test "`sqlite3 "$$scratch/td.db" "PRAGMA integrity_check"`" = "ok" || { echo "FAIL: td's store DB is not a valid SQLite file (integrity_check failed)" >&2; exit 1; }; \
	inlist=`sed "s/.*/'&'/" "$$scratch/closure.txt" | paste -sd,`; \
	live="file:/var/guix/db/db.sqlite?immutable=1"; \
	rowsql="SELECT path||'|'||hash||'|'||narSize FROM ValidPaths WHERE hash IS NOT NULL ORDER BY path"; \
	refsql="SELECT a.path||'|'||b.path FROM Refs r JOIN ValidPaths a ON r.referrer=a.id JOIN ValidPaths b ON r.reference=b.id"; \
	td_rows=`sqlite3 "$$scratch/td.db" "$$rowsql"`; \
	oracle_rows=`sqlite3 "$$live" "SELECT path||'|'||hash||'|'||narSize FROM ValidPaths WHERE path IN ($$inlist) ORDER BY path"`; \
	test -n "$$oracle_rows" || { echo "FAIL: the closure is not in the live store DB snapshot (WAL not checkpointed?)" >&2; exit 1; }; \
	test "`echo "$$td_rows" | wc -l`" = "$$n" || { echo "FAIL: td registered $$(echo "$$td_rows" | wc -l) paths, expected $$n" >&2; exit 1; }; \
	test "$$td_rows" = "$$oracle_rows" || { echo "FAIL: per-path hash/narSize differ from the daemon" >&2; echo "$$td_rows" | sed 's/^/  td:     /' >&2; echo "$$oracle_rows" | sed 's/^/  daemon: /' >&2; exit 1; }; \
	echo "   all $$n closure paths: hash + narSize match the daemon"; \
	td_refs=`sqlite3 "$$scratch/td.db" "$$refsql ORDER BY 1"`; \
	oracle_refs=`sqlite3 "$$live" "$$refsql WHERE a.path IN ($$inlist) ORDER BY 1"`; \
	test -n "$$oracle_refs" || { echo "FAIL: the daemon recorded no refs for the closure (vacuous)" >&2; exit 1; }; \
	test "$$td_refs" = "$$oracle_refs" || { echo "FAIL: the inter-path Refs relation differs from the daemon" >&2; exit 1; }; \
	echo "   the full Refs relation ($$(echo "$$td_refs" | wc -l) edges) matches the daemon"; \
	doutsql="SELECT (SELECT deriver FROM ValidPaths WHERE path='$$out')||' :: '||v.path||':'||d.id||':'||d.path FROM DerivationOutputs d JOIN ValidPaths v ON d.drv=v.id WHERE d.path='$$out'"; \
	td_dout=`sqlite3 "$$scratch/td.db" "$$doutsql"`; \
	oracle_dout=`sqlite3 "$$live" "$$doutsql"`; \
	test -n "$$oracle_dout" || { echo "FAIL: the daemon recorded no deriver/drv->output for hello (vacuous)" >&2; exit 1; }; \
	test "$$td_dout" = "$$oracle_dout" || { echo "FAIL: hello's deriver/drv->output ($$td_dout) != the daemon's ($$oracle_dout)" >&2; exit 1; }; \
	echo "   hello's deriver + drv->output mapping match the daemon"; \
	echo ">> td READS its own store DB itself (td-builder store-query — a pure-Rust SQLite reader; NO sqlite3 engine, NO daemon in td's query path):"; \
	td_read_info=`"$$tb" store-query "$$scratch/td.db" info`; \
	test "$$td_read_info" = "$$td_rows" || { echo "FAIL: td's reader disagrees with sqlite3 reading the SAME td.db bytes (info)" >&2; echo "$$td_read_info" | sed 's/^/  td-read: /' >&2; echo "$$td_rows" | sed 's/^/  sqlite3: /' >&2; exit 1; }; \
	test "$$td_read_info" = "$$oracle_rows" || { echo "FAIL: td's reader of its own DB != the daemon's record (info)" >&2; exit 1; }; \
	echo "   info: td's reader == sqlite3 (same bytes) == the daemon ($$n paths' path|hash|narSize)"; \
	td_read_refs=`"$$tb" store-query "$$scratch/td.db" references`; \
	test "$$td_read_refs" = "$$td_refs" || { echo "FAIL: td's reader disagrees with sqlite3 reading the SAME td.db bytes (references)" >&2; exit 1; }; \
	test "$$td_read_refs" = "$$oracle_refs" || { echo "FAIL: td's reader of its own DB != the daemon's Refs relation" >&2; exit 1; }; \
	echo "   references: td's reader == sqlite3 == the daemon ($$(echo "$$td_read_refs" | wc -l) edges)"; \
	rm -rf "$$scratch"; \
	echo "PASS: td WROTE the store SQLite DB for hello's full $$n-path closure itself in pure Rust AND READ it back itself (td-builder store-query — a pure-Rust SQLite reader, no sqlite3 engine and no daemon in td's own store-query path). sqlite3 PRAGMA integrity_check = ok on td's bytes; and EVERY path's hash + narSize and the full inter-path Refs relation, as answered by TD'S OWN READER, are BYTE-IDENTICAL both to sqlite3 reading the same bytes (parser oracle) and to the daemon's record (content oracle); hello's deriver/drv->output also match the daemon. registrationTime + the non-artifact per-path derivers excluded; the exact daemon schema (indexes/trigger) is a later increment."
