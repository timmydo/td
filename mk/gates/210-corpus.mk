# corpus-independence (DESIGN §7.1, Phase 2 of the §5 move-off-Guile goal). The
# CORPUS axis (where a package definition comes from), driven through the SAME
# TypeScript front-end as `ts-diff` — now declaring a PACKAGE instead of the
# system. A recipe AUTHORED in TypeScript (tests/ts/recipe-hello.ts — reconstructed
# from upstream coordinates, NOT looked up in the Guix corpus) is transpiled (tsc)
# + evaluated (boa td-ts-eval) to recipe JSON, lowered through the generic Guile
# recipe bridge (system td-recipe — the retire-last lowering target), and proven
# equal to the pinned corpus's own `hello` (the §2.5 oracle). Two legs:
#   (1) differential (tests/ts-recipe-diff.scm) — the TS recipe CONVERGES on the
#       oracle drv and a perturbed recipe (recipe-perturbed.ts, one wrong byte in
#       the source hash) DIVERGES; build-free (#:graft? #f), self-discriminating;
#   (2) build + --check (prime directive 1) — build the bridged recipe, --check it
#       reproducible (verdict-memoized — tests/check-memo.sh), assert the built
#       store object is path-identical AND NAR-hash-equal to the corpus oracle's.
# Heavy (TS toolchain + a warm hello compile + a --check), so it slots in the heavy
# pool next to the other ts gates; RE-MEASURE and RE-SORT once it has run.
HEAVY_GATES += corpus
# Not FAST_GATES: TS toolchain + a hello compile + --check — too heavy for the
# fast CI tier (absent from the small td-ci-fast image). Full check / ./check.sh.
corpus:
	@echo ">> corpus: a TypeScript-authored recipe lowers (tsc->boa->bridge) to the corpus oracle's hello; build + --check NAR-hash-equal (corpus-independence Phase 2)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$evdrv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-hello.ts"`; \
	pj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-perturbed.ts"`; \
	test -n "$$rj" -a -n "$$pj" || { echo "ERROR: ts-emit produced no recipe JSON" >&2; exit 1; }; \
	echo ">> hello recipe JSON     : $$rj"; \
	echo ">> perturbed recipe JSON : $$pj"; \
	echo ">> differential: TS recipe converges on the corpus oracle; perturbed diverges"; \
	TD_RECIPE_JSON="$$rj" TD_RECIPE_PERTURBED_JSON="$$pj" $(GUIX) repl $(LOAD) tests/ts-recipe-diff.scm; \
	echo ">> build leg: lower the bridged recipe, build, --check, NAR-equal"; \
	vars=`TD_RECIPE_JSON="$$rj" $(GUIX) repl $(LOAD) tests/ts-recipe-drv.scm 2>/dev/null`; \
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
	test -n "$$out" || { echo "ERROR: building the recipe produced no output path" >&2; exit 1; }; \
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
	echo "PASS: a TypeScript-authored recipe builds reproducibly to the corpus oracle's exact store object (NAR-hash-equal)."
