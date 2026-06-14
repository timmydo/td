# corpus-independence — own Rust builder (DESIGN §7.1, Phase 2; the §5 move-off-
# Guile goal, the "behaviorally equal where a recipe legitimately differs" case
# named in the §7.1 entry). Where `corpus` lowers the TS recipe through
# gnu-build-system (a Guile build-system + a `guile` builder), this lowers the SAME
# TS recipe through system/td-build — a raw `derivation` whose BUILDER is the
# td-builder Rust binary (`autotools-build`, builder/src/build.rs). So gnu-build-
# system and build-time Guile are GONE from the build (guix still constructs the
# .drv — the scope the human fixed 2026-06-13). The own-builder output has a
# DIFFERENT store path (own builder → own $out, which hello bakes in), so the
# differential is BEHAVIORAL, not NAR-equal:
#   • STRUCTURAL proof — the td-build derivation's builder basename is `td-builder`
#     (the Rust binary), while the corpus oracle's is `guile` (gnu-build-system);
#   • the artifact is reproducible (`guix build --check`, verdict-memoized — prime
#     directive 1);
#   • BEHAVIORAL equivalence — the td-built hello and the corpus oracle hello print
#     byte-identical output ("Hello, world!"), at DISTINCT store paths.
# Heavy (TS front-end + a hello compile + a --check + the oracle build), so it
# slots in the heavy pool next to `corpus`; RE-MEASURE and RE-SORT once it has run.
HEAVY_GATES += td-build
FAST_GATES += td-build
td-build:
	@echo ">> td-build: a TS recipe built by td's OWN Rust builder (no gnu-build-system) is reproducible and behaves identically to the corpus hello (corpus-independence)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$evdrv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-hello.ts"`; \
	test -n "$$rj" || { echo "ERROR: ts-emit produced no recipe JSON" >&2; exit 1; }; \
	echo ">> recipe JSON (TS-authored): $$rj"; \
	vars=`TD_RECIPE_JSON="$$rj" $(GUIX) repl $(LOAD) tests/td-build-drv.scm 2>/dev/null`; \
	td_drv=`printf '%s\n' "$$vars" | sed -n 's/^TD_BUILD_DRV=//p'`; \
	td_builder=`printf '%s\n' "$$vars" | sed -n 's/^TD_BUILD_BUILDER=//p'`; \
	oracle_drv=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_DRV=//p'`; \
	oracle_builder=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_BUILDER=//p'`; \
	oracle_out=`printf '%s\n' "$$vars" | sed -n 's/^ORACLE_OUT=//p'`; \
	test -n "$$td_drv" -a -n "$$oracle_drv" -a -n "$$oracle_out" \
	  || { echo "ERROR: could not lower the td-build / oracle derivations" >&2; exit 1; }; \
	echo ">> td-build drv : $$td_drv (builder: $$td_builder)"; \
	echo ">> oracle   drv : $$oracle_drv (builder: $$oracle_builder)"; \
	echo ">> STRUCTURAL proof: the builder is the Rust binary, not gnu-build-system's guile"; \
	case "$$td_builder" in td-builder*) : ;; *) echo "FAIL: td-build builder is '$$td_builder', expected the td-builder Rust binary." >&2; exit 1;; esac; \
	case "$$oracle_builder" in guile*) : ;; *) echo "FAIL: oracle builder is '$$oracle_builder', expected guile (gnu-build-system) — the contrast is not meaningful." >&2; exit 1;; esac; \
	echo ">> build the TS recipe with td's OWN Rust builder"; \
	out=`$(GUIX) build "$$td_drv"`; \
	test -n "$$out" -a -x "$$out/bin/hello" || { echo "FAIL: the td build produced no bin/hello" >&2; exit 1; }; \
	echo ">> check: reproducibility of the td-built artifact (verdict-memoized — prime directive 1)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$td_drv"; \
	echo ">> build the corpus oracle hello (gnu-build-system)"; \
	oracle_out_built=`$(GUIX) build "$$oracle_drv"`; \
	echo ">> behavioral differential: run BOTH, stdout must be byte-identical"; \
	td_say=`"$$out/bin/hello"`; \
	oracle_say=`"$$oracle_out_built/bin/hello"`; \
	echo "   td     hello -> $$td_say"; \
	echo "   oracle hello -> $$oracle_say"; \
	test "$$td_say" = "Hello, world!" || { echo "FAIL: td-built hello printed '$$td_say', expected 'Hello, world!'." >&2; exit 1; }; \
	test "$$td_say" = "$$oracle_say" || { echo "FAIL: td-built hello output differs from the corpus oracle ('$$td_say' vs '$$oracle_say')." >&2; exit 1; }; \
	echo ">> independence: the own-builder artifact is a DISTINCT store object"; \
	test "$$out" != "$$oracle_out" || { echo "FAIL: the td-built path equals the corpus oracle path — not an independent build." >&2; exit 1; }; \
	echo "PASS: a TS recipe built by td's OWN Rust builder (builder=$$td_builder, no gnu-build-system) is reproducible and prints byte-identical output to the corpus hello (gnu-build-system), at a distinct store path ($$out != $$oracle_out)."
