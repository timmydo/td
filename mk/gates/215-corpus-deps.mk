# corpus-independence — a recipe WITH build-time inputs (DESIGN §7.1, Phase 2 of
# the §5 move-off-Guile goal; the "packages with inputs" follow-on named in the
# corpus-independence entry). Where `corpus` proves a LEAF recipe (hello)
# converges, this proves a recipe with DEPENDENCIES converges: nano
# (recipe-nano.ts) declares two build inputs — gettext-minimal and ncurses — by
# their corpus package names; the generic Guile bridge (system td-recipe) RESOLVES
# each from the corpus (input resolution stays Guix's, retired LAST — §5). Two
# legs, like `corpus`, plus the inputs-specific assertions:
#   (1) differential (tests/ts-recipe-nano-diff.scm) — nano CONVERGES on the
#       corpus oracle drv; a perturbed source DIVERGES; the inputs are
#       LOAD-BEARING (stripping them diverges); and ncurses + gettext-minimal are
#       direct inputs of the lowered derivation. Build-free, self-discriminating;
#   (2) build + --check (prime directive 1) — build the bridged recipe, --check it
#       reproducible (verdict-memoized — tests/check-memo.sh), assert the built
#       store object is path-identical AND NAR-hash-equal to the corpus oracle's.
# Heavy (TS toolchain + a warm nano compile + a --check), so it slots in the heavy
# pool next to `corpus`; RE-MEASURE and RE-SORT once it has run.
HEAVY_GATES += corpus-deps
corpus-deps:
	@echo ">> corpus-deps: a TypeScript-authored recipe WITH build inputs (nano) lowers to the corpus oracle; inputs load-bearing; build + --check NAR-hash-equal (corpus-independence Phase 2)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$evdrv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-nano.ts"`; \
	pj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-nano-perturbed.ts"`; \
	test -n "$$rj" -a -n "$$pj" || { echo "ERROR: ts-emit produced no recipe JSON" >&2; exit 1; }; \
	echo ">> nano recipe JSON      : $$rj"; \
	echo ">> perturbed recipe JSON : $$pj"; \
	echo ">> differential: nano converges; perturbed + inputs-stripped diverge; input edges present"; \
	TD_RECIPE_NANO_JSON="$$rj" TD_RECIPE_NANO_PERTURBED_JSON="$$pj" $(GUIX) repl $(LOAD) tests/ts-recipe-nano-diff.scm; \
	echo ">> build leg: lower the bridged recipe, build, --check, NAR-equal"; \
	vars=`TD_RECIPE_NANO_JSON="$$rj" $(GUIX) repl $(LOAD) tests/ts-recipe-nano-drv.scm 2>/dev/null`; \
	td_drv=`printf '%s\n' "$$vars" | sed -n 's/^TD_DRV=//p'`; \
	oracle_drv=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_DRV=//p'`; \
	oracle_out=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_OUT=//p'`; \
	test -n "$$td_drv" -a -n "$$oracle_drv" -a -n "$$oracle_out" \
	  || { echo "ERROR: could not lower the recipe derivations" >&2; exit 1; }; \
	echo ">> TS recipe drv     : $$td_drv"; \
	echo ">> corpus oracle drv : $$oracle_drv"; \
	test "$$td_drv" = "$$oracle_drv" \
	  || { echo "FAIL: TS recipe drv != corpus oracle drv — convergence lost at the build-derivation level." >&2; exit 1; }; \
	echo ">> build the bridged recipe"; \
	out=`$(GUIX) build "$$td_drv"`; \
	test -n "$$out" -a -x "$$out/bin/nano" || { echo "ERROR: building the recipe produced no bin/nano" >&2; exit 1; }; \
	echo ">> check: reproducibility (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$td_drv"; \
	test "$$out" = "$$oracle_out" \
	  || { echo "FAIL: built $$out but the corpus oracle is $$oracle_out — not the same store object." >&2; exit 1; }; \
	echo ">> NAR-hash-equal (§6 metric)"; \
	nar_td=`$(GUIX) hash -S nar "$$out"`; \
	nar_or=`$(GUIX) hash -S nar "$$oracle_out"`; \
	echo "   TS recipe NAR     : $$nar_td"; \
	echo "   corpus oracle NAR : $$nar_or"; \
	test -n "$$nar_td" -a "$$nar_td" = "$$nar_or" \
	  || { echo "FAIL: TS recipe NAR hash != corpus oracle NAR hash." >&2; exit 1; }; \
	echo "PASS: a TypeScript-authored recipe WITH build inputs (nano) builds reproducibly to the corpus oracle's exact store object (NAR-hash-equal), with its declared inputs load-bearing."
