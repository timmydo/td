# input-recipes — reconstruct an individual INPUT recipe (DESIGN §7.1 move-off-Guile
# §5; the "reconstruct individual input recipes" follow-on to the now-done
# input-resolution track, toolchain retired LAST). Where `corpus`/`corpus-deps`
# reconstruct the TOP package (hello/nano), this reconstructs one of nano's INPUTS —
# pkg-config (ncurses's native-input) — store-path-equal to the corpus oracle, so its
# resolution can be backed by td's OWN recipe instead of Guile's specification->package
# (one package off the resolver; the toolchain stays Guile, §5). pkg-config is the
# configureFlags + multi-URI rung: it exercises the two recipe-DSL firsts the bridge
# now carries (both flow through the boa evaluator's generic JSON capture). Legs:
#   (1) differential (tests/ts-recipe-pkgconfig-diff.scm) — pkg-config CONVERGES on
#       the corpus oracle drv; a perturbed configure flag DIVERGES; the configureFlags
#       are LOAD-BEARING (stripping them diverges); the multi-URI source is LOAD-BEARING
#       (collapsing it to one URL diverges). Build-free, self-discriminating;
#   (2) build + --check (prime directive 1) — build the bridged recipe, --check it
#       reproducible (verdict-memoized — tests/check-memo.sh), assert the built store
#       object is path-identical AND NAR-hash-equal to the corpus oracle's.
# Heavy (TS toolchain + a pkg-config build + a --check), so it slots in the heavy pool
# next to `corpus`/`corpus-deps`; RE-MEASURE and RE-SORT once it has run.
HEAVY_GATES += corpus-pkgconfig
corpus-pkgconfig:
	@echo ">> corpus-pkgconfig: a TS-authored recipe for an INPUT package (pkg-config, configure-flags + multi-URI source) lowers store-path-equal to the corpus oracle; flags + URIs load-bearing; build + --check NAR-hash-equal (input-recipes: reconstruct individual input recipes)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$evdrv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-pkg-config.ts"`; \
	pj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-pkg-config-perturbed.ts"`; \
	test -n "$$rj" -a -n "$$pj" || { echo "ERROR: ts-emit produced no recipe JSON" >&2; exit 1; }; \
	echo ">> pkg-config recipe JSON : $$rj"; \
	echo ">> perturbed recipe JSON  : $$pj"; \
	echo ">> differential: pkg-config converges; perturbed flag + flags-stripped + single-URI diverge"; \
	TD_RECIPE_PKGCONFIG_JSON="$$rj" TD_RECIPE_PKGCONFIG_PERTURBED_JSON="$$pj" $(GUIX) repl $(LOAD) tests/ts-recipe-pkgconfig-diff.scm; \
	echo ">> build leg: lower the bridged recipe, build, --check, NAR-equal"; \
	vars=`TD_RECIPE_PKGCONFIG_JSON="$$rj" $(GUIX) repl $(LOAD) tests/ts-recipe-pkgconfig-drv.scm 2>/dev/null`; \
	td_drv=`printf '%s\n' "$$vars" | sed -n 's/^TD_DRV=//p'`; \
	oracle_drv=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_DRV=//p'`; \
	oracle_out=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_OUT=//p'`; \
	test -n "$$td_drv" -a -n "$$oracle_drv" -a -n "$$oracle_out" \
	  || { echo "ERROR: could not lower the recipe derivations" >&2; exit 1; }; \
	echo ">> [MIGRATION ORACLE — removable when Guix is retired] TS recipe drv == corpus oracle drv"; \
	echo ">> TS recipe drv     : $$td_drv"; \
	echo ">> corpus oracle drv : $$oracle_drv"; \
	test "$$td_drv" = "$$oracle_drv" \
	  || { echo "FAIL: TS recipe drv != corpus oracle drv — convergence lost at the build-derivation level." >&2; exit 1; }; \
	echo ">> build the bridged recipe"; \
	out=`$(GUIX) build "$$td_drv"`; \
	test -n "$$out" -a -x "$$out/bin/pkg-config" || { echo "ERROR: building the recipe produced no bin/pkg-config" >&2; exit 1; }; \
	echo ">> [DURABLE: behavioral] the built pkg-config runs and reports its version — no Guix oracle involved"; \
	ver=`"$$out/bin/pkg-config" --version`; \
	test "$$ver" = "0.29.2" \
	  || { echo "FAIL: the built pkg-config --version reported '$$ver', expected 0.29.2 — the artifact does not function." >&2; exit 1; }; \
	echo "   pkg-config --version -> $$ver"; \
	echo ">> [DURABLE: reproducibility] td computes the verdict ITSELF — td-builder check double-builds the recipe .drv in independent userns sandboxes (no guix build --check)"; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	printf '%s\n' "$$vars" | sed -n 's/^TD_IN=//p' > "$(CURDIR)/.tdck-pkgconfig.in"; \
	TD_GUIX="$(GUIX)" sh tests/td-check-repro.sh "$$tb" "$$td_drv" "$(CURDIR)/.tdck-pkgconfig.in" "$(CURDIR)/.tdck-pkgconfig"; \
	rm -f "$(CURDIR)/.tdck-pkgconfig.in"; \
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
	echo "PASS: the built pkg-config runs (--version 0.29.2, durable behavioral merit) and is reproducible by td's OWN double-build (td-builder check, no Guix in that verdict); configure flags + multi-URI source are load-bearing; and (migration oracle) it is byte-identical to the corpus pkg-config and guix build --check agrees."
