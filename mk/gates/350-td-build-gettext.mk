# td-build-gettext — gettext-minimal built by td's OWN Rust builder (DESIGN §7.1
# move-off-Guile §5; the gnu-build-system-fidelity step that widens the own-builder
# corpus coverage toward retiring td-recipe.scm). Where `td-build-corpus` (345)
# routes the SIMPLE recipes (popt, libatomic-ops) through `system/td-build`, this
# routes the MOST ELABORATE one — gettext-minimal: build inputs
# (libunistring/libxml2/ncurses), configure flags, a makeFlag, and TWO custom phases
# that exercise the FULL phase-body vocabulary (findFiles, cons, letWhich,
# withFluids, format, stringAppend). td's Rust phase runner (builder/src/build.rs)
# applies all of it after unpack — no gnu-build-system, no Guile in the build — and
# the autotools build then configures/makes/installs against the real inputs.
#
# This is the gate that exercised — and a `find_files` fix that unblocked — td's
# phase runner on a real tree: `(find-files DIR REGEX)` over gettext-tools/tests
# (where most files DON'T match) previously failed the build (the trailing
# non-match left the helper's `while` pipeline at grep's exit 1 under `set -e`);
# the build COMPLETING is the durable proof the runner now handles findFiles/cons
# without error. The own-builder output is a DISTINCT store path (own builder, and
# td builds a single `out` where the corpus splits a `doc`), so — like 345 — there
# is NO Guix byte-identity leg: every assertion is DURABLE and nothing here needs
# rewriting when Guix is retired.
#   STRUCTURAL  : builder = td-builder (the Rust binary), not gnu-build-system's guile;
#   DURABLE behavioral   : the built gettext-tools RUN (msgfmt + xgettext --version),
#                          which also proves the phase runner applied the phases
#                          (the build would have failed in-phase otherwise);
#   DURABLE reproducibility : td-builder check double-builds the .drv (no guix --check);
#   INDEPENDENCE : distinct store path from the corpus gettext-minimal.
# Heavy (TS toolchain + a gettext build + td's double-build of a large package), so
# it slots last in the heavy pool; RE-MEASURE and RE-SORT once it has run.
HEAVY_GATES += td-build-gettext
td-build-gettext:
	@echo ">> td-build-gettext: nano's input gettext-minimal (inputs + configure flags + makeFlag + two full-vocabulary phases) built by td's OWN Rust builder (phases in Rust, no gnu-build-system); behavioral + reproducible + distinct from the corpus (move-off-Guile §5)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$evdrv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" -a -x "$$tb" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval / td-builder" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-gettext-minimal.ts"`; \
	test -n "$$rj" || { echo "ERROR: ts-emit produced no JSON for gettext-minimal" >&2; exit 1; }; \
	vars=`TD_RECIPE_JSON="$$rj" $(GUIX) repl $(LOAD) tests/td-build-recipe-drv.scm 2>/dev/null`; \
	td_drv=`printf '%s\n' "$$vars" | sed -n 's/^TD_DRV=//p'`; \
	td_out=`printf '%s\n' "$$vars" | sed -n 's/^TD_OUT=//p'`; \
	td_builder=`printf '%s\n' "$$vars" | sed -n 's/^TD_BUILDER=//p'`; \
	test -n "$$td_drv" -a -n "$$td_out" -a -n "$$td_builder" || { echo "ERROR: could not lower gettext-minimal via td-build" >&2; exit 1; }; \
	echo ">> td-build drv : $$td_drv"; \
	echo ">> [STRUCTURAL] builder is td's Rust binary, not gnu-build-system's guile: $$td_builder"; \
	case "$$td_builder" in td-builder) : ;; *) echo "FAIL: gettext-minimal builder is '$$td_builder', expected td-builder." >&2; exit 1;; esac; \
	echo ">> build gettext-minimal with td's OWN builder (configure flags + makeFlag + full-vocabulary phases, no gnu-build-system)"; \
	out=`$(GUIX) build "$$td_drv"`; \
	test -n "$$out" -a "$$out" = "$$td_out" || { echo "FAIL: gettext-minimal td build produced no/incorrect out ($$out vs $$td_out)" >&2; exit 1; }; \
	echo ">> [DURABLE: behavioral] the built gettext-tools run — msgfmt + xgettext --version — no Guix oracle (also proves td's phase runner applied the phases: the build would have failed in-phase otherwise)"; \
	test -x "$$out/bin/msgfmt" -a -x "$$out/bin/xgettext" || { echo "FAIL: td-built gettext-minimal missing bin/msgfmt or bin/xgettext." >&2; exit 1; }; \
	mver=`"$$out/bin/msgfmt" --version | head -n1`; \
	xver=`"$$out/bin/xgettext" --version | head -n1`; \
	echo "   $$mver"; echo "   $$xver"; \
	printf '%s' "$$mver" | grep -q "0.23.1" || { echo "FAIL: msgfmt --version did not report 0.23.1 (got: '$$mver')." >&2; exit 1; }; \
	printf '%s' "$$xver" | grep -q "0.23.1" || { echo "FAIL: xgettext --version did not report 0.23.1 (got: '$$xver')." >&2; exit 1; }; \
	echo ">> [DURABLE: reproducibility] td-builder check double-builds gettext-minimal's .drv (no guix build --check)"; \
	printf '%s\n' "$$vars" | sed -n 's/^TD_IN=//p' > "$(CURDIR)/.tdck-gettext.in"; \
	TD_GUIX="$(GUIX)" sh tests/td-check-repro.sh "$$tb" "$$td_drv" "$(CURDIR)/.tdck-gettext.in" "$(CURDIR)/.tdck-gettext"; \
	rm -f "$(CURDIR)/.tdck-gettext.in"; \
	echo ">> [INDEPENDENCE] distinct from the corpus gettext-minimal (own builder → own path)"; \
	corpus_out=`$(GUIX) build gettext-minimal 2>/dev/null | grep -- "-gettext-minimal-" | grep -v -- "-doc" | head -n1 || true`; \
	if [ -n "$$corpus_out" ] && [ "$$out" = "$$corpus_out" ]; then echo "FAIL: td-built gettext-minimal path equals the corpus path." >&2; exit 1; fi; \
	echo "   td-built gettext-minimal: $$out (distinct from corpus)"; \
	echo "PASS: td's OWN Rust builder built gettext-minimal — nano's most elaborate input (build inputs + configure flags + makeFlag + two phases exercising the full phase-body vocabulary: findFiles/cons/letWhich/withFluids/format/stringAppend) — with NO gnu-build-system / Guile in the build; the gettext-tools run (durable behavioral), the build is reproducible by td's own double-build, and it lands at a distinct store path from the corpus build. td's phase runner now handles a real findFiles/cons tree — td-recipe.scm is replaceable for gettext too."
