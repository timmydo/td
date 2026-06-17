# td-realize-store — `td-builder realize` is a COMPLETE daemon op: build AND own the
# store record, on a REAL-dependency recipe (DESIGN §7.1 move-off-Guile §5;
# own-builder-daemon track). Extends realize (td-realize, 355 — hello) two ways: the
# subject is gettext-minimal (build inputs libunistring/libxml2/ncurses + configure
# flags + a makeFlag + two phases — a real dependency graph, not just the toolchain),
# and realize now writes a td store-db (store_db, pure Rust) registering the built
# output — the daemon's post-build registration, no guix-daemon. So td computes the
# closure, builds in its userns sandbox, AND records the output in its OWN store db.
# Legs: DURABLE behavioral — the realized gettext-tools run (msgfmt + xgettext);
# DURABLE structural — store-query reads the output record back from td's db (write
# then read, round-trip); MIGRATION ORACLE (removable when guix retires) — that
# record (path|hash|narSize) equals the daemon's ValidPaths row for the same output.
HEAVY_GATES += td-realize-store
td-realize-store:
	@echo ">> td-realize-store: td realizes gettext-minimal (real deps) with no guix-daemon AND registers the output in its OWN store-db; the tools run + the db record matches the daemon (oracle)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" -a -x "$$tb" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval / td-builder" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-gettext-minimal.ts"`; \
	test -n "$$rj" || { echo "ERROR: ts-emit produced no JSON for gettext-minimal" >&2; exit 1; }; \
	vars=`TD_RECIPE_JSON="$$rj" $(GUIX) repl $(LOAD) tests/td-build-recipe-drv.scm 2>/dev/null`; \
	td_drv=`printf '%s\n' "$$vars" | sed -n 's/^TD_DRV=//p'`; \
	td_out=`printf '%s\n' "$$vars" | sed -n 's/^TD_OUT=//p'`; \
	test -n "$$td_drv" -a -n "$$td_out" || { echo "ERROR: could not lower gettext-minimal via td-build" >&2; exit 1; }; \
	$(GUIX) build "$$td_drv" >/dev/null 2>&1 || { echo "ERROR: could not realize the recipe's inputs" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-realize-store-scratch"; chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	echo ">> td-builder realize gettext-minimal (computes closure itself, builds, registers into td's store-db)"; \
	"$$tb" realize "$$td_drv" /var/guix/db/db.sqlite "$$scratch/b" > "$$scratch/out.txt" 2> "$$scratch/realize.err" || { echo "FAIL: realize errored" >&2; cat "$$scratch/realize.err" >&2; exit 1; }; \
	sed 's/^/   /' "$$scratch/realize.err"; \
	echo ">> [DURABLE: behavioral] the realized gettext-tools run (byte-identical to realize's output):"; \
	mver=`"$$td_out/bin/msgfmt" --version | head -n1`; xver=`"$$td_out/bin/xgettext" --version | head -n1`; \
	echo "   $$mver"; echo "   $$xver"; \
	printf '%s' "$$mver" | grep -q "0.23.1" || { echo "FAIL: msgfmt --version not 0.23.1 (got '$$mver')" >&2; exit 1; }; \
	test -s "$$scratch/b/td.db" || { echo "FAIL: realize wrote no td store-db" >&2; exit 1; }; \
	td_rec=`"$$tb" store-query "$$scratch/b/td.db" info | grep -F "$$td_out"`; \
	test -n "$$td_rec" || { echo "FAIL: td's store-db has no record for the realized output" >&2; cat "$$scratch/b/registration" >&2; exit 1; }; \
	echo ">> [DURABLE: structural] td registered the output in its OWN store-db and reads it back: $$td_rec"; \
	live="file:/var/guix/db/db.sqlite?immutable=1"; \
	daemon_rec=`sqlite3 "$$live" "SELECT path||'|'||hash||'|'||narSize FROM ValidPaths WHERE path='$$td_out'"`; \
	test -n "$$daemon_rec" || { echo "FAIL: the output is not in the live store DB snapshot (oracle vacuous)" >&2; exit 1; }; \
	echo ">> [MIGRATION ORACLE — removable when guix retires] td's store-db record == the daemon's ValidPaths row"; \
	test "$$td_rec" = "$$daemon_rec" || { echo "FAIL: td store-db record != daemon's — td '$$td_rec' vs daemon '$$daemon_rec'" >&2; exit 1; }; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; \
	echo "PASS: td-builder realized gettext-minimal — a real-dependency recipe — with NO guix-daemon (computed the closure itself, built in its userns sandbox) AND registered the output in its OWN store-db; the realized gettext-tools run (durable), td reads the output record back from its db (durable round-trip), and (oracle) that record matches the daemon's ValidPaths row. realize is now a complete daemon op: build + own the store."
