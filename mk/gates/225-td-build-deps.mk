# corpus-independence — own Rust builder, a recipe WITH build inputs (DESIGN §7.1,
# Phase 2; the "packages with inputs" follow-on for the own-builder path). Where
# `td-build` builds a LEAF recipe (hello) with td's OWN Rust builder, this builds a
# recipe with DEPENDENCIES: nano (recipe-nano.ts) declares ncurses + gettext-minimal;
# system/td-build resolves them from the corpus (input resolution stays Guix's,
# retired LAST — §5) and feeds their include/lib dirs to the Rust autotools-build via
# TD_INPUTS, so td's OWN builder links real deps — no gnu-build-system, no build-side
# Guile. Proven:
#   • STRUCTURAL — the td-build derivation's builder basename is `td-builder` (Rust),
#     while the corpus oracle's is `guile` (gnu-build-system);
#   • INPUT-EDGE — ncurses + gettext-minimal are DIRECT inputs of the td-build
#     derivation (the declared deps actually entered the own-builder build);
#   • REPRODUCIBLE — `guix build --check` (verdict-memoized — prime directive 1);
#   • BEHAVIORAL — the td-built nano and the corpus nano print byte-identical
#     `--version` (incl. the ncursesw-driven "--enable-utf8" feature detection — proof
#     the dep linked + ran), at DISTINCT store paths (own builder).
# Heavy (TS front-end + a nano compile + a --check + the oracle build), next to
# `td-build`; RE-MEASURE and RE-SORT once it has run.
HEAVY_GATES += td-build-deps
td-build-deps:
	@echo ">> td-build-deps: a TS recipe WITH build inputs (nano) built by td's OWN Rust builder (no gnu-build-system) links its deps, is reproducible, and behaves identically to the corpus nano (corpus-independence)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$evdrv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-nano.ts"`; \
	test -n "$$rj" || { echo "ERROR: ts-emit produced no recipe JSON" >&2; exit 1; }; \
	echo ">> recipe JSON (TS-authored): $$rj"; \
	vars=`TD_RECIPE_NANO_JSON="$$rj" $(GUIX) repl $(LOAD) tests/td-build-deps-drv.scm 2>/dev/null`; \
	td_drv=`printf '%s\n' "$$vars" | sed -n 's/^TD_BUILD_DRV=//p'`; \
	td_builder=`printf '%s\n' "$$vars" | sed -n 's/^TD_BUILD_BUILDER=//p'`; \
	has_ncurses=`printf '%s\n' "$$vars" | sed -n 's/^TD_HAS_NCURSES=//p'`; \
	has_gettext=`printf '%s\n' "$$vars" | sed -n 's/^TD_HAS_GETTEXT=//p'`; \
	oracle_drv=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_DRV=//p'`; \
	oracle_builder=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_BUILDER=//p'`; \
	oracle_out=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_OUT=//p'`; \
	test -n "$$td_drv" -a -n "$$oracle_drv" -a -n "$$oracle_out" \
	  || { echo "ERROR: could not lower the td-build / oracle derivations" >&2; exit 1; }; \
	echo ">> td-build drv : $$td_drv (builder: $$td_builder)"; \
	echo ">> oracle   drv : $$oracle_drv (builder: $$oracle_builder)"; \
	echo ">> STRUCTURAL proof: the builder is the Rust binary, not gnu-build-system's guile"; \
	case "$$td_builder" in td-builder*) : ;; *) echo "FAIL: td-build builder is '$$td_builder', expected the td-builder Rust binary." >&2; exit 1;; esac; \
	case "$$oracle_builder" in guile*) : ;; *) echo "FAIL: oracle builder is '$$oracle_builder', expected guile (gnu-build-system)." >&2; exit 1;; esac; \
	echo ">> INPUT-EDGE proof: the declared deps are direct inputs of the td-build derivation"; \
	test "$$has_ncurses" = "yes" || { echo "FAIL: ncurses is not a direct input of the td-build nano derivation — the declared input did not enter the own-builder build." >&2; exit 1; }; \
	test "$$has_gettext" = "yes" || { echo "FAIL: gettext is not a direct input of the td-build nano derivation." >&2; exit 1; }; \
	echo "   ncurses + gettext-minimal are direct inputs of the td-build derivation"; \
	echo ">> build the TS recipe with td's OWN Rust builder"; \
	out=`$(GUIX) build "$$td_drv"`; \
	test -n "$$out" -a -x "$$out/bin/nano" || { echo "FAIL: the td build produced no bin/nano" >&2; exit 1; }; \
	echo ">> check: reproducibility of the td-built artifact (verdict-memoized — prime directive 1)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$td_drv"; \
	echo ">> build the corpus oracle nano (gnu-build-system)"; \
	oracle_out_built=`$(GUIX) build "$$oracle_drv"`; \
	echo ">> behavioral differential: run BOTH, --version must be byte-identical"; \
	td_ver=`"$$out/bin/nano" --version`; \
	oracle_ver=`"$$oracle_out_built/bin/nano" --version`; \
	printf '   td     nano --version: %s\n' "`printf '%s' "$$td_ver" | head -n1`"; \
	printf '   oracle nano --version: %s\n' "`printf '%s' "$$oracle_ver" | head -n1`"; \
	printf '%s' "$$td_ver" | head -n1 | grep -q "GNU nano, version 8.7.1" || { echo "FAIL: td-built nano did not report the expected version." >&2; exit 1; }; \
	test "$$td_ver" = "$$oracle_ver" || { echo "FAIL: td-built nano --version differs from the corpus oracle — not behaviorally identical." >&2; exit 1; }; \
	echo ">> independence: the own-builder artifact is a DISTINCT store object"; \
	test "$$out" != "$$oracle_out" || { echo "FAIL: the td-built path equals the corpus oracle path — not an independent build." >&2; exit 1; }; \
	echo "PASS: a TS recipe WITH build inputs (nano) built by td's OWN Rust builder (builder=$$td_builder, no gnu-build-system) links its declared deps (ncurses + gettext-minimal are direct inputs), is reproducible, and prints byte-identical --version to the corpus nano, at a distinct store path ($$out != $$oracle_out)."
