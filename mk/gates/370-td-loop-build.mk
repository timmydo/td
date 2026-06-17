# td-loop-build — the loop BUILDS a recipe with td's OWN builder and CONSUMES td's
# output (DESIGN §7.1 move-off-Guile §5; own-builder-daemon — "a builder the loop uses
# instead of guix-daemon"). The realize gates so far built via td then ran the daemon's
# byte-identical /gnu/store copy; here the loop runs the artifact FROM td's OWN store
# output (the realize scratch store), so the consumed binary is td's build, not
# guix-daemon's. Subject: gettext-minimal (real deps). DURABLE: the loop runs msgfmt
# from td's own store output (a path under td's scratch store, NOT /gnu/store).
# MIGRATION ORACLE (removable when guix retires): td's output is byte-identical (NAR)
# to the daemon's build of the same drv. guix-daemon builds only the inputs (toolchain,
# retired last) + the oracle copy; td builds + serves the recipe the loop consumes.
HEAVY_GATES += td-loop-build
td-loop-build:
	@echo ">> td-loop-build: the loop builds gettext-minimal with td's OWN builder (realize) and RUNS msgfmt from td's OWN store output — guix-daemon is only the oracle"
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
	$(GUIX) build "$$td_drv" >/dev/null 2>&1 || { echo "ERROR: could not realize the recipe's inputs / oracle" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-loop-build-scratch"; chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	echo ">> the loop builds gettext-minimal with td's OWN builder (realize)"; \
	"$$tb" realize "$$td_drv" /var/guix/db/db.sqlite "$$scratch/b" > "$$scratch/out.txt" 2> "$$scratch/realize.err" || { echo "FAIL: td realize failed" >&2; cat "$$scratch/realize.err" >&2; exit 1; }; \
	nsout="$$scratch/b/newstore/`basename "$$td_out"`"; \
	case "$$nsout" in "$$scratch"/*) : ;; *) echo "FAIL: td's output path $$nsout is not under td's scratch store" >&2; exit 1;; esac; \
	test -x "$$nsout/bin/msgfmt" || { echo "FAIL: td's own store output has no bin/msgfmt — the loop has nothing td-built to consume" >&2; exit 1; }; \
	ver=`"$$nsout/bin/msgfmt" --version | head -n1`; \
	echo ">> [DURABLE] the loop ran msgfmt from td's OWN store output (not /gnu/store):"; \
	echo "   $$nsout/bin/msgfmt -> $$ver"; \
	printf '%s' "$$ver" | grep -q "0.23.1" || { echo "FAIL: td-built msgfmt did not report 0.23.1 (got '$$ver')" >&2; exit 1; }; \
	echo ">> [MIGRATION ORACLE — removable when guix retires] td's own-store output is NAR-identical to the daemon's build"; \
	td_nar=`"$$tb" nar-hash "$$nsout"`; \
	oracle_nar=`"$$tb" nar-hash "$$td_out"`; \
	test -n "$$td_nar" -a "$$td_nar" = "$$oracle_nar" || { echo "FAIL: td's own-store output NAR $$td_nar != the daemon's $$oracle_nar" >&2; exit 1; }; \
	echo "   td own-store NAR == daemon NAR ($$td_nar)"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; \
	echo "PASS: the loop built gettext-minimal with td's OWN builder and CONSUMED td's output — it ran msgfmt from td's own store output (a path under td's scratch store, not /gnu/store; DURABLE), which is NAR-identical to the daemon's build (oracle). guix-daemon built only the inputs + the oracle copy; td built and served the recipe the loop used."
