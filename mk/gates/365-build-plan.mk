# build-plan — td CHAINS a td-built dependency into a downstream build: grep links
# the pcre2 td just built, not guix's (DESIGN §7.1 move-off-Guile §5). The
# per-package locks could not express this edge — `corpus-no-guix` builds grep's
# own derivation Guile-free but its pcre2 input is still GUIX's
# (tests/grep-no-guix.lock). Here a `td-recipe-output` lock entry marks pcre2 as a
# dep td builds + substitutes: `td-builder build-plan` realizes pcre2, then grep
# with td's pcre2 in its inputs (closure spanning td.db ∪ guix's db, the dep staged
# from a td-store). Subject pcre2 → grep (both ends already td-built:
# corpus-deps-no-guix + the toolchain set); ncurses → nano follows once ncurses is
# reconstructed — same machinery.
#
# DURABLE structural: grep's assembled .drv references td's pcre2 output AND NOT
# guix's pcre2 (the substitution is load-bearing — both legs red if it no-ops).
# DURABLE behavioral: grep runs from td's own output and a `grep -P` PCRE match
# works, so td's pcre2 is actually loaded. DURABLE reproducibility: `td-builder
# check` double-builds grep (staging td's pcre2 from the on-disk path closure.txt
# records) bit-identically. MIGRATION ORACLE:
# td's grep + pcre2 land at distinct store paths from guix's. Built with guix/Guile
# SCRUBBED FROM PATH; the toolchain + locks are the guix-built SEED (§5, last).
HEAVY_GATES += build-plan
build-plan:
	@echo ">> build-plan: pcre2 -> grep — grep's .drv references td's pcre2 (NOT guix's); grep runs + matches with td's pcre2, reproducibly, at distinct paths"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	ev=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-ts-eval)'`/bin/td-ts-eval; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$ev" -a -x "$$tb" -a -x "$$node" -a -n "$$tsc" || { echo "ERROR: could not resolve node / tsc / ts-eval / td-builder" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	cu=`grep -- '-coreutils-' "$(CURDIR)/tests/grep-no-guix.lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	root="$(CURDIR)/.td-build-cache/build-plan"; mkdir -p "$$root/tmp"; \
	sh tests/ts-emit.sh tests/ts/recipe-pcre2.ts > "$$root/pcre2.json"; \
	sh tests/ts-emit.sh tests/ts/recipe-grep.ts  > "$$root/grep.json"; \
	test -s "$$root/pcre2.json" -a -s "$$root/grep.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
	guix_pcre2=`sed -n 's/^pcre2 //p' "$(CURDIR)/tests/grep-no-guix.lock"`; \
	test -n "$$guix_pcre2" || { echo "ERROR: no pcre2 line in grep-no-guix.lock" >&2; exit 1; }; \
	sed 's#^pcre2 .*#pcre2 '"$$guix_pcre2"' td-recipe-output#' "$(CURDIR)/tests/grep-no-guix.lock" > "$$root/grep-chained.lock"; \
	grep -q 'td-recipe-output' "$$root/grep-chained.lock" || { echo "ERROR: failed to mark pcre2 as a td-recipe-output edge" >&2; exit 1; }; \
	{ grep ' /gnu/store/' "$(CURDIR)/tests/pcre2-no-guix.lock"; grep ' /gnu/store/' "$$root/grep-chained.lock" | grep -v 'td-recipe-output'; } | sed 's/^[^ ]* //' | sort -u | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the guix-built seeds" >&2; exit 1; }; \
	printf 'step %s %s\nstep %s %s\n' "$$root/pcre2.json" "$(CURDIR)/tests/pcre2-no-guix.lock" "$$root/grep.json" "$$root/grep-chained.lock" > "$$root/plan"; \
	env -i HOME="$$root" TMPDIR="$$root/tmp" PATH="$$cu/bin" "$$tb" build-plan "$$root/plan" /var/guix/db/db.sqlite "$$root" > "$$root/out" 2>"$$root/err" || { echo "FAIL: build-plan (guix/Guile off PATH):" >&2; tail -30 "$$root/err" >&2; exit 1; }; \
	td_pcre2=`sed -n 's/^STEP pcre2 //p' "$$root/out"`; \
	td_grep=`sed -n 's/^STEP grep //p' "$$root/out"`; \
	test -n "$$td_pcre2" -a -n "$$td_grep" || { echo "FAIL: build-plan did not report both steps" >&2; cat "$$root/out" >&2; exit 1; }; \
	echo "  built: pcre2=$$td_pcre2  grep=$$td_grep"; \
	gdrv="$$root/grep/grep-3.11.drv"; \
	test -s "$$gdrv" || { echo "FAIL: grep's assembled .drv is missing ($$gdrv)" >&2; exit 1; }; \
	grep -q "$$td_pcre2" "$$gdrv" || { echo "FAIL: grep's .drv does NOT reference td's pcre2 ($$td_pcre2)" >&2; exit 1; }; \
	if grep -q "$$guix_pcre2" "$$gdrv"; then echo "FAIL: grep's .drv STILL references guix's pcre2 ($$guix_pcre2) — the td-recipe-output substitution did not happen" >&2; exit 1; fi; \
	echo "  [DURABLE structural] grep's .drv references td's pcre2 and NOT guix's ($$guix_pcre2)"; \
	gout="$$root/grep/newstore/`basename "$$td_grep"`"; \
	pl="$$root/tdstore/`basename "$$td_pcre2"`/lib"; \
	test -x "$$gout/bin/grep" || { echo "FAIL: grep binary missing from td's output" >&2; exit 1; }; \
	LD_LIBRARY_PATH="$$pl" "$$gout/bin/grep" --version | grep -q 'grep (GNU grep) 3.11' || { echo "FAIL: td's grep --version != 3.11" >&2; exit 1; }; \
	printf 'foobar\nbaz\n' | LD_LIBRARY_PATH="$$pl" "$$gout/bin/grep" -P 'o{2}' | grep -qx foobar || { echo "FAIL: grep -P (PCRE via td's pcre2) did not match" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] td's grep runs (3.11) and a grep -P PCRE match works — td's pcre2 is loaded"; \
	if [ -f "$$root/grep/repro-ok" ] && [ "$$root/grep/repro-ok" -nt "$$gdrv" ]; then \
	  echo "  [DURABLE repro] CACHED: grep drv unchanged + previously verified reproducible — check skipped"; \
	else \
	  rm -rf "$$root/grep/chk"; \
	  env -i HOME="$$root" TMPDIR="$$root/tmp" PATH="$$cu/bin" "$$tb" check "$$gdrv" "$$root/grep/closure.txt" "$$root/grep/chk" >/dev/null 2>"$$root/chkerr" || { echo "FAIL: chained grep NOT reproducible (td-builder check):" >&2; tail -6 "$$root/chkerr" >&2; exit 1; }; \
	  touch "$$root/grep/repro-ok"; \
	  echo "  [DURABLE repro] td-builder check double-build agrees the chained grep is reproducible"; \
	fi; \
	gg=`$(GUIX) build grep 2>/dev/null | grep -v -- '-debug' | head -1 || true`; \
	gp=`$(GUIX) build pcre2 2>/dev/null | grep -v -- '-debug\|-doc\|-static' | head -1 || true`; \
	if [ -n "$$gg" ] && [ "$$td_grep" = "$$gg" ]; then echo "FAIL: td's grep path equals guix's" >&2; exit 1; fi; \
	if [ -n "$$gp" ] && [ "$$td_pcre2" = "$$gp" ]; then echo "FAIL: td's pcre2 path equals guix's" >&2; exit 1; fi; \
	echo "  [MIGRATION ORACLE] td's grep + pcre2 land at distinct paths from guix's"; \
	echo "PASS: build-plan chained pcre2 -> grep — grep's .drv references td's OWN pcre2 (not guix's), grep runs + PCRE-matches from td's output with td's pcre2 loaded (durable), the chained build is reproducible by td's own double-build (durable), and both land at distinct store paths from guix's (own, then diverge). Built with guix/Guile SCRUBBED FROM PATH; the toolchain + locks are the guix-built seed (§5, retired last)."
