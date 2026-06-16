# input-recipes — reconstruct nano's DIRECT input gettext-minimal (DESIGN §7.1
# move-off-Guile §5; the recipe frontier, follow-on to corpus-gzip). The most
# elaborate recipe yet: a `doc` output, a makeFlag, configure flags, build inputs
# (libunistring/libxml2/ncurses), and TWO custom phases — patch-fixed-paths (literal
# substitute* over file lists) and patch-tests (the full phase-body vocabulary: a
# match variable, a let-which binding, a with-fluids byte-encoding guard, find-files,
# cons, and a format replacement). The bridge lowers all of it to the byte-identical
# `(modify-phases …)` gexp the corpus writes by hand — so reconstructing it
# store-path-equal starts retiring the resolver for a real nano dependency. Legs:
#   (1) differential (tests/ts-recipe-gettext-diff.scm) — gettext-minimal CONVERGES
#       on the corpus oracle drv; a perturbed source DIVERGES; the phases are
#       LOAD-BEARING (stripping them diverges). Build-free, self-discriminating;
#   (2) build + DURABLE legs (no Guix oracle): the built gettext-tools run (`msgfmt
#       --version`), AND the build is reproducible by td's OWN double-build
#       (`td-builder check`, tests/td-check-repro.sh; no guix build --check);
#   (3) MIGRATION ORACLE (removable when Guix is retired): the drv + out are
#       byte-identical (path + NAR) to the corpus gettext-minimal, and `guix build
#       --check` agrees the .drv is reproducible.
# Heavy (TS toolchain + a gettext build + td's double-build of a large package), so
# it slots last in the heavy pool; RE-MEASURE and RE-SORT once it has run.
HEAVY_GATES += corpus-gettext
corpus-gettext:
	@echo ">> corpus-gettext: a TS-authored recipe for nano's input gettext-minimal (doc output + makeFlags + configure flags + two custom phases, full phase-body vocabulary) lowers store-path-equal to the corpus oracle; phases load-bearing; runs + reproducible by td (input-recipes)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$evdrv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-gettext-minimal.ts"`; \
	pj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-gettext-minimal-perturbed.ts"`; \
	test -n "$$rj" -a -n "$$pj" || { echo "ERROR: ts-emit produced no recipe JSON" >&2; exit 1; }; \
	echo ">> differential: gettext-minimal converges; perturbed source + phases-stripped diverge"; \
	TD_RECIPE_GETTEXT_JSON="$$rj" TD_RECIPE_GETTEXT_PERTURBED_JSON="$$pj" $(GUIX) repl $(LOAD) tests/ts-recipe-gettext-diff.scm; \
	echo ">> build leg: lower the bridged recipe, build, behavioral + td-check, NAR-equal"; \
	vars=`TD_RECIPE_GETTEXT_JSON="$$rj" $(GUIX) repl $(LOAD) tests/ts-recipe-gettext-drv.scm 2>/dev/null`; \
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
	  || { echo "FAIL: TS recipe drv != corpus oracle drv — the generated phase-body gexp is not byte-identical to the corpus phases." >&2; exit 1; }; \
	echo ">> build the bridged recipe (realizes out + doc)"; \
	$(GUIX) build "$$td_drv" >/dev/null || { echo "ERROR: building the recipe failed" >&2; exit 1; }; \
	test -d "$$td_out" -a -x "$$td_out/bin/msgfmt" || { echo "ERROR: the out output $$td_out has no bin/msgfmt" >&2; exit 1; }; \
	echo ">> the recipe's out output : $$td_out"; \
	echo ">> [DURABLE: behavioral] the built gettext-tools run — msgfmt --version — no Guix oracle involved"; \
	ver=`"$$td_out/bin/msgfmt" --version | head -n1`; \
	echo "   $$ver"; \
	printf '%s' "$$ver" | grep -q "0.23.1" \
	  || { echo "FAIL: the built msgfmt --version did not report 0.23.1 (got: '$$ver') — the artifact does not function." >&2; exit 1; }; \
	echo ">> [DURABLE: reproducibility] td computes the verdict ITSELF — td-builder check double-builds the recipe .drv in independent userns sandboxes (no guix build --check)"; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	printf '%s\n' "$$vars" | sed -n 's/^TD_IN=//p' > "$(CURDIR)/.tdck-gettext.in"; \
	TD_GUIX="$(GUIX)" sh tests/td-check-repro.sh "$$tb" "$$td_drv" "$(CURDIR)/.tdck-gettext.in" "$(CURDIR)/.tdck-gettext"; \
	rm -f "$(CURDIR)/.tdck-gettext.in"; \
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
	echo "PASS: a TypeScript-authored recipe for nano's direct input gettext-minimal (doc output + makeFlags + configure flags + two custom phases) builds; its gettext-tools run (msgfmt --version, durable behavioral) and it is reproducible by td's OWN double-build (td-builder check, no Guix); the phases are load-bearing; and (migration oracle) it is byte-identical to the corpus gettext-minimal and guix build --check agrees."
