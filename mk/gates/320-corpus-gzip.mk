# input-recipes — a phase that bakes a build STORE PATH (DESIGN §7.1 move-off-Guile
# §5; the phase frontier, follow-on to corpus-popt). Where corpus-popt proves a
# phase with literal / (which …) substitutions, this proves the path-reference
# idiom: gzip (recipe-gzip.ts) rewrites `exec 'gzip'` to `exec <out>/bin/gzip` via a
# string-append with an (assoc-ref outputs "out") part, in a (lambda* (#:key outputs
# …) …), and builds with #:tests? #f. So this rung lands two recipe-DSL capabilities
# — `tests` (#:tests?) and a `stringAppend` substitution replacement with
# `{output}`/`{input}` parts — the idioms nano's DIRECT inputs (ncurses,
# gettext-minimal) use to inject store paths in their phases. Legs:
#   (1) differential (tests/ts-recipe-gzip-diff.scm) — gzip CONVERGES on the corpus
#       oracle drv; a perturbed source DIVERGES; the declared phase is LOAD-BEARING
#       (stripping `phases` diverges). Build-free, self-discriminating;
#   (2) build + DURABLE legs (no Guix oracle): the built gzip round-trips a file
#       (behavioral), AND it is reproducible by td's OWN double-build (`td-builder
#       check` builds the .drv twice in independent userns sandboxes — prime
#       directive 1 on td's terms, no `guix build --check` in that verdict);
#   (3) MIGRATION ORACLE (removable when Guix is retired): the drv + out are
#       byte-identical (path + NAR) to the corpus gzip, and `guix build --check`
#       agrees the .drv is reproducible.
# Heavy (TS toolchain + a gzip build + td's double-build), so it slots in the heavy
# pool next to corpus-popt; RE-MEASURE and RE-SORT once it has run.
HEAVY_GATES += corpus-gzip
corpus-gzip:
	@echo ">> corpus-gzip: a TS-authored recipe whose phase bakes a build store path (gzip, string-append + assoc-ref outputs, #:tests? #f) lowers store-path-equal to the corpus oracle; phase load-bearing; build + --check NAR-hash-equal (input-recipes)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$evdrv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-gzip.ts"`; \
	pj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-gzip-perturbed.ts"`; \
	test -n "$$rj" -a -n "$$pj" || { echo "ERROR: ts-emit produced no recipe JSON" >&2; exit 1; }; \
	echo ">> gzip recipe JSON      : $$rj"; \
	echo ">> perturbed recipe JSON : $$pj"; \
	echo ">> differential: gzip converges; perturbed source + phases-stripped diverge"; \
	TD_RECIPE_GZIP_JSON="$$rj" TD_RECIPE_GZIP_PERTURBED_JSON="$$pj" $(GUIX) repl $(LOAD) tests/ts-recipe-gzip-diff.scm; \
	echo ">> build leg: lower the bridged recipe, build, --check, NAR-equal"; \
	vars=`TD_RECIPE_GZIP_JSON="$$rj" $(GUIX) repl $(LOAD) tests/ts-recipe-gzip-drv.scm 2>/dev/null`; \
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
	test -n "$$out" -a -x "$$out/bin/gzip" || { echo "ERROR: building the recipe produced no bin/gzip" >&2; exit 1; }; \
	echo ">> [DURABLE: behavioral] the built gzip round-trips a file (compress | decompress) — no Guix oracle involved"; \
	rt=`printf 'td-gzip-roundtrip\n' | "$$out/bin/gzip" -c | "$$out/bin/gzip" -dc`; \
	test "$$rt" = "td-gzip-roundtrip" \
	  || { echo "FAIL: the built gzip did not round-trip a file (got: '$$rt') — the artifact does not function." >&2; exit 1; }; \
	echo "   gzip -c | gzip -dc round-trip OK"; \
	echo ">> [DURABLE: reproducibility] td computes the verdict ITSELF — td-builder check builds the recipe .drv TWICE in independent userns sandboxes and compares per-output NAR hashes; no guix build --check in this verdict"; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	sc="$(CURDIR)/.corpus-gzip-tdcheck"; chmod -R u+w "$$sc" 2>/dev/null || true; rm -rf "$$sc"; mkdir -p "$$sc"; \
	{ printf '%s\n' "$$vars" | sed -n 's/^TD_IN=//p'; echo "$$td_drv"; } | xargs $(GUIX) gc -R | sort -u > "$$sc/paths.txt"; \
	echo "   staged build closure: `wc -l < "$$sc/paths.txt"` store items"; \
	"$$tb" check "$$td_drv" "$$sc/paths.txt" "$$sc/c" > "$$sc/out.txt" 2>"$$sc/err.txt" \
	  || { echo "FAIL: td-builder check reported NON-reproducible (or errored):" >&2; cat "$$sc/out.txt" "$$sc/err.txt" >&2; chmod -R u+w "$$sc" 2>/dev/null || true; rm -rf "$$sc"; exit 1; }; \
	grep -F "$$out" "$$sc/out.txt" | grep -q "reproducible" \
	  || { echo "FAIL: td-builder check did not report $$out reproducible:" >&2; cat "$$sc/out.txt" >&2; chmod -R u+w "$$sc" 2>/dev/null || true; rm -rf "$$sc"; exit 1; }; \
	echo "   td double-build agrees: $$out is reproducible (td's OWN verdict, no Guix)"; \
	chmod -R u+w "$$sc" 2>/dev/null || true; rm -rf "$$sc"; \
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
	echo "PASS: the built gzip round-trips a file (durable behavioral merit) and is reproducible by td's OWN double-build (td-builder check, no Guix in that verdict); the store-path-baking phase is load-bearing; and (migration oracle) it is byte-identical to the corpus gzip and guix build --check agrees."
