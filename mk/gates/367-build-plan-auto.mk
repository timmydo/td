# build-plan-auto — td GENERATES the build plan from the recipe GRAPH, no hand-written
# plan or manifest. `td-builder build-plan --auto bash` reads recipe-bash.ts's declared
# inputs, recursively resolves the owned ones (readline -> ncurses), topo-sorts, marks
# each owned edge `td-recipe-output`, and builds the DAG. Proves the auto path produces
# the SAME edge-owned result the manifest-driven gate (365) does — bash built from td's
# OWN readline + ncurses (a 2-level DAG) — derived, not enumerated. Shares 365's cache
# root, so the builds reuse 365's outputs. guix/Guile SCRUBBED FROM PATH (§5 seed).
HEAVY_GATES += build-plan-auto
build-plan-auto:
	@echo ">> build-plan-auto: td-builder build-plan --auto bash — derive bash<-readline<-ncurses from the recipe graph (no manifest), bash's .drv references td's deps"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	ev=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-ts-eval)'`/bin/td-ts-eval; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$ev" -a -x "$$tb" -a -x "$$node" -a -n "$$tsc" || { echo "ERROR: could not resolve node / tsc / ts-eval / td-builder" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	cu=`grep -- '-coreutils-' "$(CURDIR)/tests/bash-no-guix.lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	root="$(CURDIR)/.td-build-cache/build-plan"; jd="$$root/auto-json"; mkdir -p "$$jd" "$$root/tmp"; \
	for r in bash readline ncurses; do \
	  sh tests/ts-emit.sh "tests/ts/recipe-$$r.ts" > "$$jd/$$r.json"; \
	  test -s "$$jd/$$r.json" || { echo "ERROR: ts-emit produced no JSON for $$r" >&2; exit 1; }; \
	done; \
	{ for r in bash readline ncurses; do grep ' /gnu/store/' "$(CURDIR)/tests/$$r-no-guix.lock"; done; } | sed 's/^[^ ]* //' | sort -u | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize guix seeds" >&2; exit 1; }; \
	env -i HOME="$$root" TMPDIR="$$root/tmp" PATH="$$cu/bin" "$$tb" build-plan --auto bash "$$jd" "$(CURDIR)/tests" /var/guix/db/db.sqlite "$$root" > "$$root/auto-out" 2>"$$root/auto-err" || { echo "FAIL: build-plan --auto bash (guix/Guile off PATH):" >&2; tail -30 "$$root/auto-err" >&2; exit 1; }; \
	grep -q 'derived a 3-step plan' "$$root/auto-err" || { echo "FAIL: --auto did not derive a 3-step plan (bash<-readline<-ncurses)" >&2; grep 'derived' "$$root/auto-err" >&2 || true; exit 1; }; \
	grep -qE 'recipe graph:.*ncurses.*readline.*bash' "$$root/auto-err" || { echo "FAIL: --auto topo order is not ncurses -> readline -> bash" >&2; grep 'recipe graph' "$$root/auto-err" >&2 || true; exit 1; }; \
	echo "  [DURABLE structural] --auto derived the DAG from the recipe graph: ncurses -> readline -> bash"; \
	td_bash=`sed -n 's/^STEP bash //p' "$$root/auto-out"`; \
	td_rl=`sed -n 's/^STEP readline //p' "$$root/auto-out"`; \
	td_nc=`sed -n 's/^STEP ncurses //p' "$$root/auto-out"`; \
	test -n "$$td_bash" -a -n "$$td_rl" -a -n "$$td_nc" || { echo "FAIL: --auto did not report all three STEP outputs" >&2; cat "$$root/auto-out" >&2; exit 1; }; \
	bdrv=`ls "$$root/bash"/*.drv 2>/dev/null | head -1`; \
	test -s "$$bdrv" || { echo "FAIL: bash .drv missing" >&2; exit 1; }; \
	grep -q "$$td_rl" "$$bdrv" || { echo "FAIL: bash's .drv does NOT reference td's readline ($$td_rl)" >&2; exit 1; }; \
	grep -q "$$td_nc" "$$bdrv" || { echo "FAIL: bash's .drv does NOT reference td's ncurses ($$td_nc)" >&2; exit 1; }; \
	gnc=`sed -n "s#^[^ ]*-ncurses-[^ ]* \(/gnu/store/[^ ]*\)#\1#p" "$(CURDIR)/tests/bash-no-guix.lock" | head -1`; \
	if [ -n "$$gnc" ] && grep -q "$$gnc" "$$bdrv"; then echo "FAIL: bash's .drv STILL references guix's ncurses ($$gnc)" >&2; exit 1; fi; \
	echo "  [DURABLE structural] bash's .drv references td's readline + ncurses and NOT guix's"; \
	bout="$$root/bash/newstore/`basename "$$td_bash"`"; \
	ld="$$root/tdstore/`basename "$$td_rl"`/lib:$$root/tdstore/`basename "$$td_nc"`/lib"; \
	LD_LIBRARY_PATH="$$ld" "$$bout/bin/bash" -c 'echo $$BASH_VERSION' | grep -q '^5' || { echo "FAIL: td's bash (auto-derived) did not run" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] td's auto-derived bash runs, loading td's readline + ncurses"; \
	echo "PASS: build-plan --auto GENERATED the chain from the recipe graph (no manifest): bash <- td's readline <- td's ncurses, derived by topo-sorting recipe-bash.ts's declared inputs; bash's .drv references td's OWN readline + ncurses (not guix's) and the binary runs. As the owned set grows, a recipe's edges chain with no hand-written plan. guix/Guile SCRUBBED FROM PATH; toolchain + locks are the guix-built seed (§5, retired last)."
