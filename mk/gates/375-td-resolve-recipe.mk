# td-resolve-recipe — the resolver's lock entry for a recipe-backed dep comes from td's
# OWN recipe, not Guile's specification->package (DESIGN §7.1 move-off-Guile §5; the
# "retire the resolver" axis, package-by-package, toolchain last). resolve-lock.scm
# generates the pinned lock via specification->package(NAME); this generates
# gettext-minimal's entry by lowering td's recipe (recipe-gettext-minimal.ts via
# system/td-build) to its out path — so the NAME->path resolution is td's RECIPE, with
# no specification->package(gettext-minimal). `td-builder resolve` then returns that
# td-recipe path (consumption, as in the `resolve` gate).
#   STRUCTURAL: the entry's drv is built by td-builder (the recipe's own-builder drv);
#   SELF-DISCRIMINATION (durable): it DIVERGES from specification->package's path —
#     proof it is genuinely td's recipe, not guix's package (own, then diverge); the
#     resolved gettext is td's own (built + functioning per td-build-gettext / td-realize-store);
#   MIGRATION ORACLE: ncurses (no td recipe yet) still resolves via Guile, unchanged —
#     the package-by-package boundary.
# Additive: the pinned tests/td-build-inputs.lock + nano's build are untouched.
HEAVY_GATES += td-resolve-recipe
td-resolve-recipe:
	@echo ">> td-resolve-recipe: gettext-minimal's resolver lock entry comes from td's OWN recipe (td-build lowering), not specification->package; td-builder resolve returns it; it diverges from Guile's package resolution (own, then diverge)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" -a -x "$$tb" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval / td-builder" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	echo ">> lower gettext-minimal via td's recipe (no specification->package for the name)"; \
	rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-gettext-minimal.ts"`; \
	test -n "$$rj" || { echo "ERROR: ts-emit produced no JSON for gettext-minimal" >&2; exit 1; }; \
	vars=`TD_RECIPE_JSON="$$rj" $(GUIX) repl $(LOAD) tests/td-build-recipe-drv.scm 2>/dev/null`; \
	td_out=`printf '%s\n' "$$vars" | sed -n 's/^TD_OUT=//p'`; \
	td_builder=`printf '%s\n' "$$vars" | sed -n 's/^TD_BUILDER=//p'`; \
	test -n "$$td_out" -a -n "$$td_builder" || { echo "ERROR: could not lower gettext-minimal via td-build" >&2; exit 1; }; \
	echo ">> [STRUCTURAL] the entry's drv builder is td's Rust binary, not gnu-build-system's guile: $$td_builder"; \
	case "$$td_builder" in td-builder) : ;; *) echo "FAIL: gettext-minimal entry builder is '$$td_builder', expected td-builder." >&2; exit 1;; esac; \
	scratch="$(CURDIR)/.td-resolve-recipe-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	printf 'gettext-minimal %s\n' "$$td_out" > "$$scratch/td-recipe.lock"; \
	echo ">> td-builder resolve gettext-minimal from the td-recipe-sourced lock (no Guile):"; \
	td_resolved=`"$$tb" resolve "$$scratch/td-recipe.lock" gettext-minimal`; \
	test "$$td_resolved" = "$$td_out" || { echo "FAIL: td resolve returned '$$td_resolved', expected the td-recipe path '$$td_out'" >&2; exit 1; }; \
	echo "   gettext-minimal -> $$td_resolved (from td's recipe)"; \
	echo ">> [SELF-DISCRIMINATION] Guile's specification->package resolves gettext-minimal to a DIFFERENT path (own, then diverge)"; \
	guile_path=`$(GUIX) repl $(LOAD) tests/resolve-lock.scm gettext-minimal 2>/dev/null | sed -n 's/^gettext-minimal //p'`; \
	test -n "$$guile_path" || { echo "ERROR: Guile resolution produced nothing" >&2; exit 1; }; \
	echo "   specification->package: $$guile_path"; \
	test "$$td_out" != "$$guile_path" || { echo "FAIL: td's recipe path equals specification->package's — no divergence, the entry is not genuinely td's recipe." >&2; exit 1; }; \
	echo ">> [MIGRATION ORACLE] ncurses (no td recipe yet) still resolves via Guile — the package-by-package boundary"; \
	test ! -f "$(CURDIR)/tests/ts/recipe-ncurses.ts" || { echo "NOTE: an ncurses recipe now exists — extend this gate." >&2; }; \
	ncurses_path=`$(GUIX) repl $(LOAD) tests/resolve-lock.scm ncurses 2>/dev/null | sed -n 's/^ncurses //p'`; \
	test -n "$$ncurses_path" || { echo "ERROR: Guile ncurses resolution produced nothing" >&2; exit 1; }; \
	echo "   ncurses -> $$ncurses_path (Guile, retired when reconstructed)"; \
	rm -rf "$$scratch"; \
	echo "PASS: gettext-minimal's resolver lock entry was generated from td's OWN recipe (td-build lowering, builder=td-builder, NO specification->package for the name); td-builder resolve returns it; it DIVERGES from Guile's specification->package path (own, then diverge — genuinely td's recipe, the resolved gettext is td's own); ncurses, lacking a td recipe, still resolves via Guile (package-by-package, toolchain last). First step retiring the resolver."
