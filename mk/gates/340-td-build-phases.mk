# td-build-phases — td's OWN builder runs a recipe's custom PHASES in Rust (DESIGN
# §7.1 move-off-Guile §5; the step toward td owning .drv creation: td's builder,
# not gnu-build-system's Guile, runs the phases). Where corpus-gzip lowers the gzip
# recipe through gnu-build-system (byte-identical to Guix), this lowers the SAME
# recipe through system/td-build — builder = the td-builder Rust binary — and the
# recipe's `use-absolute-name-of-gzip` phase (an output-baking substitute*) is
# applied by td's OWN phase runner (builder/src/build.rs + json.rs), with NO Guile
# in the build. The own-builder output has a DISTINCT store path, so this is proven
# BEHAVIORALLY + structurally + reproducibly, not by NAR-equality. Legs:
#   (1) STRUCTURAL — the derivation's builder basename is `td-builder`, not `guile`;
#   (2) DURABLE behavioral — td's runner applied the phase: the installed gunzip
#       execs the absolute `<out>/bin/gzip` (not `'gzip'`), AND gzip round-trips a
#       file (compress | decompress). No Guix oracle in either;
#   (3) DURABLE reproducibility — `td-builder check` double-build (tests/td-check-
#       repro.sh; no `guix build --check` in the verdict);
#   (4) INDEPENDENCE — the artifact is at a DISTINCT path from the corpus gzip.
# Heavy (TS front-end + a td-builder compile + a gzip build via td + td's
# double-build); slots in the heavy pool by the other td-build gates.
HEAVY_GATES += td-build-phases
td-build-phases:
	@echo ">> td-build-phases: td's OWN builder runs the recipe's phase in Rust — builds gzip applying use-absolute-name-of-gzip (no gnu-build-system), the phase's effect is observable, reproducible by td (move-off-Guile §5)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$evdrv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" -a -x "$$tb" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval / td-builder" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-gzip.ts"`; \
	test -n "$$rj" || { echo "ERROR: ts-emit produced no recipe JSON" >&2; exit 1; }; \
	echo ">> lower gzip through td's OWN builder (system td-build), with its phase"; \
	vars=`TD_RECIPE_GZIP_JSON="$$rj" $(GUIX) repl $(LOAD) tests/td-build-phases-drv.scm 2>/dev/null`; \
	td_drv=`printf '%s\n' "$$vars" | sed -n 's/^TD_DRV=//p'`; \
	td_out=`printf '%s\n' "$$vars" | sed -n 's/^TD_OUT=//p'`; \
	td_builder=`printf '%s\n' "$$vars" | sed -n 's/^TD_BUILDER=//p'`; \
	test -n "$$td_drv" -a -n "$$td_out" -a -n "$$td_builder" || { echo "ERROR: could not lower the td-build derivation" >&2; exit 1; }; \
	echo ">> td-build drv : $$td_drv"; \
	echo ">> [STRUCTURAL] the derivation's builder is td's Rust binary, not gnu-build-system's guile"; \
	echo "   builder: $$td_builder"; \
	case "$$td_builder" in td-builder) : ;; *) echo "FAIL: builder is '$$td_builder', expected td-builder — this is not the own-builder path." >&2; exit 1;; esac; \
	echo ">> build gzip with td's OWN builder (it applies the phase in Rust)"; \
	out=`$(GUIX) build "$$td_drv"`; \
	test -n "$$out" -a -x "$$out/bin/gzip" || { echo "ERROR: td's build produced no bin/gzip" >&2; exit 1; }; \
	test "$$out" = "$$td_out" || { echo "ERROR: built $$out != lowered $$td_out" >&2; exit 1; }; \
	echo ">> [DURABLE: behavioral] td's phase runner applied use-absolute-name-of-gzip — gunzip execs the absolute <out>/bin/gzip"; \
	grep -qF "exec $$out/bin/gzip" "$$out/bin/gunzip" \
	  || { echo "FAIL: the installed gunzip does not exec $$out/bin/gzip — td's phase runner did not apply the phase:" >&2; grep -n 'exec' "$$out/bin/gunzip" | head >&2; exit 1; }; \
	echo "   gunzip: `grep -F "exec $$out/bin/gzip" "$$out/bin/gunzip" | head -n1`"; \
	echo ">> [DURABLE: behavioral] the built gzip round-trips a file"; \
	rt=`printf 'td-build-phases\n' | "$$out/bin/gzip" -c | "$$out/bin/gzip" -dc`; \
	test "$$rt" = "td-build-phases" || { echo "FAIL: the td-built gzip did not round-trip (got: '$$rt')." >&2; exit 1; }; \
	echo "   gzip -c | gzip -dc round-trip OK"; \
	echo ">> [DURABLE: reproducibility] td-builder check double-builds the .drv (no guix build --check)"; \
	printf '%s\n' "$$vars" | sed -n 's/^TD_IN=//p' > "$(CURDIR)/.tdck-tdbuildphases.in"; \
	TD_GUIX="$(GUIX)" sh tests/td-check-repro.sh "$$tb" "$$td_drv" "$(CURDIR)/.tdck-tdbuildphases.in" "$(CURDIR)/.tdck-tdbuildphases"; \
	rm -f "$(CURDIR)/.tdck-tdbuildphases.in"; \
	echo ">> [INDEPENDENCE] distinct from the corpus gzip (own builder → own path)"; \
	corpus_out=`$(GUIX) build gzip 2>/dev/null | grep -- '-gzip-1.14' | head -n1 || true`; \
	if [ -n "$$corpus_out" ] && [ "$$out" = "$$corpus_out" ]; then echo "FAIL: td-built path equals the corpus gzip path — expected a distinct own-builder artifact." >&2; exit 1; fi; \
	echo "PASS: td's OWN Rust builder built gzip and applied the recipe's use-absolute-name-of-gzip phase itself (no gnu-build-system, no Guile in the build) — the phase's effect (gunzip execs <out>/bin/gzip) is observable, gzip round-trips (durable behavioral), the build is reproducible by td's own double-build, and the artifact is at a distinct store path from the corpus gzip."
