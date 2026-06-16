# input-recipes — the MULTI-OUTPUT recipe rung (DESIGN §7.1 move-off-Guile §5; the
# "reconstruct individual input recipes" frontier, follow-on to corpus-pkgconfig).
# Where `corpus-pkgconfig` adds the configureFlags + multi-URI recipe-DSL firsts,
# this adds the `outputs` field: libatomic-ops (recipe-libatomic-ops.ts) splits a
# `debug` output off `out`, and that extra output enters the build derivation.
# libatomic-ops is the cleanest demonstrator — NO configure-flags, NO custom phases,
# so the extra output is the only thing beyond a leaf recipe. It is the prerequisite
# capability for nano's DIRECT inputs ncurses + gettext-minimal, which both carry a
# `doc` output (plus phases, a later rung). Legs:
#   (1) differential (tests/ts-recipe-libatomic-diff.scm) — libatomic-ops CONVERGES
#       on the corpus oracle drv; a perturbed source DIVERGES; the declared outputs
#       are LOAD-BEARING (stripping them diverges); the lowered derivation declares
#       BOTH outputs (out + debug). Build-free, self-discriminating;
#   (2) build + --check (prime directive 1) — build the bridged recipe, --check it
#       reproducible (verdict-memoized — tests/check-memo.sh), assert the built
#       `out` store object is path-identical AND NAR-hash-equal to the corpus oracle's.
# Heavy (TS toolchain + a libatomic-ops build + a --check), so it slots in the heavy
# pool next to `corpus-pkgconfig`; RE-MEASURE and RE-SORT once it has run.
HEAVY_GATES += corpus-libatomic
corpus-libatomic:
	@echo ">> corpus-libatomic: a TS-authored MULTI-OUTPUT recipe (libatomic-ops, out + debug) lowers store-path-equal to the corpus oracle; the extra output is load-bearing; build + --check NAR-hash-equal (input-recipes: reconstruct individual recipes)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$evdrv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-libatomic-ops.ts"`; \
	pj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-libatomic-ops-perturbed.ts"`; \
	test -n "$$rj" -a -n "$$pj" || { echo "ERROR: ts-emit produced no recipe JSON" >&2; exit 1; }; \
	echo ">> libatomic-ops recipe JSON : $$rj"; \
	echo ">> perturbed recipe JSON     : $$pj"; \
	echo ">> differential: libatomic-ops converges; perturbed source + outputs-stripped diverge; both outputs present"; \
	TD_RECIPE_LIBATOMIC_JSON="$$rj" TD_RECIPE_LIBATOMIC_PERTURBED_JSON="$$pj" $(GUIX) repl $(LOAD) tests/ts-recipe-libatomic-diff.scm; \
	echo ">> build leg: lower the bridged recipe, build, --check, NAR-equal"; \
	vars=`TD_RECIPE_LIBATOMIC_JSON="$$rj" $(GUIX) repl $(LOAD) tests/ts-recipe-libatomic-drv.scm 2>/dev/null`; \
	td_drv=`printf '%s\n' "$$vars" | sed -n 's/^TD_DRV=//p'`; \
	oracle_drv=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_DRV=//p'`; \
	td_out=`printf '%s\n' "$$vars" | sed -n 's/^TD_OUT=//p'`; \
	oracle_out=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_OUT=//p'`; \
	test -n "$$td_drv" -a -n "$$oracle_drv" -a -n "$$td_out" -a -n "$$oracle_out" \
	  || { echo "ERROR: could not lower the recipe derivations" >&2; exit 1; }; \
	echo ">> [MIGRATION ORACLE — removable when Guix is retired] TS recipe drv == corpus oracle drv"; \
	echo ">> TS recipe drv     : $$td_drv"; \
	echo ">> corpus oracle drv : $$oracle_drv"; \
	test "$$td_drv" = "$$oracle_drv" \
	  || { echo "FAIL: TS recipe drv != corpus oracle drv — convergence lost at the build-derivation level." >&2; exit 1; }; \
	echo ">> build the bridged recipe (realizes both outputs: out + debug)"; \
	$(GUIX) build "$$td_drv" >/dev/null || { echo "ERROR: building the recipe failed" >&2; exit 1; }; \
	test -d "$$td_out" || { echo "ERROR: the out output $$td_out was not realized" >&2; exit 1; }; \
	echo ">> the recipe's out output : $$td_out"; \
	echo ">> [DURABLE: structural] the built out has the library + header it exists to ship — no Guix oracle involved"; \
	test -f "$$td_out/lib/libatomic_ops.a" -a -f "$$td_out/include/atomic_ops.h" \
	  || { echo "FAIL: the built libatomic-ops out is missing lib/libatomic_ops.a or include/atomic_ops.h — the artifact has the wrong shape." >&2; exit 1; }; \
	echo "   lib/libatomic_ops.a + include/atomic_ops.h present"; \
	echo ">> [DURABLE: reproducibility] td computes the verdict ITSELF — td-builder check double-builds the recipe .drv in independent userns sandboxes (both outputs out + debug; no guix build --check)"; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	printf '%s\n' "$$vars" | sed -n 's/^TD_IN=//p' > "$(CURDIR)/.tdck-libatomic.in"; \
	TD_GUIX="$(GUIX)" sh tests/td-check-repro.sh "$$tb" "$$td_drv" "$(CURDIR)/.tdck-libatomic.in" "$(CURDIR)/.tdck-libatomic"; \
	rm -f "$(CURDIR)/.tdck-libatomic.in"; \
	echo ">> [MIGRATION ORACLE — removable when Guix is retired] guix build --check agrees the .drv is reproducible (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$td_drv"; \
	echo ">> [MIGRATION ORACLE — removable when Guix is retired] the built out == the corpus oracle's out (path + NAR)"; \
	test "$$td_out" = "$$oracle_out" \
	  || { echo "FAIL: the recipe's out output $$td_out != the corpus oracle out $$oracle_out — not the same store object." >&2; exit 1; }; \
	nar_td=`$(GUIX) hash -S nar "$$td_out"`; \
	nar_or=`$(GUIX) hash -S nar "$$oracle_out"`; \
	echo "   TS recipe NAR     : $$nar_td"; \
	echo "   corpus oracle NAR : $$nar_or"; \
	test -n "$$nar_td" -a "$$nar_td" = "$$nar_or" \
	  || { echo "FAIL: TS recipe NAR hash != corpus oracle NAR hash." >&2; exit 1; }; \
	echo "PASS: the built libatomic-ops out ships lib/libatomic_ops.a + include/atomic_ops.h (durable structural merit) and is reproducible by td's OWN double-build (td-builder check, both outputs, no Guix in that verdict); the extra output is load-bearing; and (migration oracle) it is byte-identical to the corpus libatomic-ops and guix build --check agrees."
