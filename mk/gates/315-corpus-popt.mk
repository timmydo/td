# input-recipes — the PHASES recipe rung (DESIGN §7.1 move-off-Guile §5; the phase
# frontier for nano's own inputs, follow-on to corpus-pkgconfig/corpus-libatomic).
# Where the earlier rungs add configureFlags + multi-URI + multi-output, this adds
# the recipe-DSL `phases` field: popt (recipe-popt.ts) adds a custom `patch-test`
# phase (two substitute* source patches) before `configure`. popt is the cleanest
# phase demonstrator — its ONLY non-default argument is that one phase (no
# configure-flags, no extra outputs, no inputs), so the phase capability is
# isolated. The bridge lowers the phase DATA to the byte-identical
# `(modify-phases %standard-phases (add-before 'configure 'patch-test (lambda _ …)))`
# gexp the corpus writes by hand — the prerequisite capability for nano's DIRECT
# inputs ncurses + gettext-minimal, whose recipes patch source files in custom
# phases. Legs:
#   (1) differential (tests/ts-recipe-popt-diff.scm) — popt CONVERGES on the corpus
#       oracle drv; a perturbed source DIVERGES; the declared phase is LOAD-BEARING
#       (stripping `phases` diverges). Build-free, self-discriminating;
#   (2) build + --check (prime directive 1) — build the bridged recipe, --check it
#       reproducible (verdict-memoized — tests/check-memo.sh), assert the built
#       store object is path-identical AND NAR-hash-equal to the corpus oracle's.
# Heavy (TS toolchain + a popt build + a --check), so it slots in the heavy pool
# next to corpus-pkgconfig/corpus-libatomic; RE-MEASURE and RE-SORT once it has run.
HEAVY_GATES += corpus-popt
corpus-popt:
	@echo ">> corpus-popt: a TS-authored recipe with a custom build PHASE (popt) lowers store-path-equal to the corpus oracle; the phase is load-bearing; build + --check NAR-hash-equal (input-recipes: reconstruct individual recipes)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$evdrv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-popt.ts"`; \
	pj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-popt-perturbed.ts"`; \
	test -n "$$rj" -a -n "$$pj" || { echo "ERROR: ts-emit produced no recipe JSON" >&2; exit 1; }; \
	echo ">> popt recipe JSON      : $$rj"; \
	echo ">> perturbed recipe JSON : $$pj"; \
	echo ">> differential: popt converges; perturbed source + phases-stripped diverge"; \
	TD_RECIPE_POPT_JSON="$$rj" TD_RECIPE_POPT_PERTURBED_JSON="$$pj" $(GUIX) repl $(LOAD) tests/ts-recipe-popt-diff.scm; \
	echo ">> build leg: lower the bridged recipe, build, --check, NAR-equal"; \
	vars=`TD_RECIPE_POPT_JSON="$$rj" $(GUIX) repl $(LOAD) tests/ts-recipe-popt-drv.scm 2>/dev/null`; \
	td_drv=`printf '%s\n' "$$vars" | sed -n 's/^TD_DRV=//p'`; \
	oracle_drv=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_DRV=//p'`; \
	oracle_out=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_OUT=//p'`; \
	test -n "$$td_drv" -a -n "$$oracle_drv" -a -n "$$oracle_out" \
	  || { echo "ERROR: could not lower the recipe derivations" >&2; exit 1; }; \
	echo ">> [MIGRATION ORACLE — removable when Guix is retired] TS recipe drv == corpus oracle drv"; \
	echo ">> TS recipe drv     : $$td_drv"; \
	echo ">> corpus oracle drv : $$oracle_drv"; \
	test "$$td_drv" = "$$oracle_drv" \
	  || { echo "FAIL: TS recipe drv != corpus oracle drv — the generated phase gexp is not byte-identical to the corpus phase." >&2; exit 1; }; \
	echo ">> build the bridged recipe"; \
	out=`$(GUIX) build "$$td_drv"`; \
	test -n "$$out" -a -d "$$out/lib" || { echo "ERROR: building the recipe produced no lib output" >&2; exit 1; }; \
	echo ">> [DURABLE: structural] the built out ships the shared library + header popt exists to provide — no Guix oracle involved"; \
	test -f "$$out/lib/libpopt.so" -a -f "$$out/include/popt.h" \
	  || { echo "FAIL: the built popt out is missing lib/libpopt.so or include/popt.h — the artifact has the wrong shape." >&2; exit 1; }; \
	echo "   lib/libpopt.so + include/popt.h present"; \
	echo ">> [DURABLE: reproducibility] td computes the verdict ITSELF — td-builder check double-builds the recipe .drv in independent userns sandboxes (no guix build --check)"; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	printf '%s\n' "$$vars" | sed -n 's/^TD_IN=//p' > "$(CURDIR)/.tdck-popt.in"; \
	TD_GUIX="$(GUIX)" sh tests/td-check-repro.sh "$$tb" "$$td_drv" "$(CURDIR)/.tdck-popt.in" "$(CURDIR)/.tdck-popt"; \
	rm -f "$(CURDIR)/.tdck-popt.in"; \
	echo ">> [MIGRATION ORACLE — removable when Guix is retired] guix build --check agrees the .drv is reproducible (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$td_drv"; \
	echo ">> [MIGRATION ORACLE — removable when Guix is retired] the built out == the corpus oracle's out (path + NAR)"; \
	test "$$out" = "$$oracle_out" \
	  || { echo "FAIL: built $$out but the corpus oracle is $$oracle_out — not the same store object." >&2; exit 1; }; \
	nar_td=`$(GUIX) hash -S nar "$$out"`; \
	nar_or=`$(GUIX) hash -S nar "$$oracle_out"`; \
	echo "   TS recipe NAR     : $$nar_td"; \
	echo "   corpus oracle NAR : $$nar_or"; \
	test -n "$$nar_td" -a "$$nar_td" = "$$nar_or" \
	  || { echo "FAIL: TS recipe NAR hash != corpus oracle NAR hash." >&2; exit 1; }; \
	echo "PASS: the built popt ships lib/libpopt.so + include/popt.h (durable structural merit) and is reproducible by td's OWN double-build (td-builder check, no Guix in that verdict); the phase DATA lowered to the byte-identical modify-phases gexp and is load-bearing; and (migration oracle) it is byte-identical to the corpus popt and guix build --check agrees."
