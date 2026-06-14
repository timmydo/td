# input-resolution (DESIGN §7.1 move-off-Guile; "retire input resolution", the §5
# toolchain layer retired LAST). system/td-build.scm resolves a recipe's inputs to
# store paths via Guile's specification->package -> package-derivation -> out path.
# This gate is the FIRST decoupling step, done the loop-sandbox/td-check way —
# ADDITIVE EQUIVALENCE before any swap (directive 3: the build is untouched):
# `td-builder resolve` looks up the SAME inputs from a PINNED lock
# (tests/td-build-inputs.lock) with NO Guile, and the gate proves td's lock
# resolution is store-path-EQUAL to Guile's LIVE resolution (the oracle,
# tests/resolve-lock.scm) for the nano recipe's declared inputs (ncurses +
# gettext-minimal). What moves to Rust is the lock CONSUMPTION; the RESOLVER that
# computes the lock stays Guile, retired package-by-package later (§5). Heavy only
# for the warm td-builder compile (no VM); a perturbed lock diverges (verified-red).
HEAVY_GATES += resolve
resolve:
	@echo ">> resolve: td-builder resolves recipe inputs from a pinned lock, store-path-equal to Guile's live specification->package resolution (input-resolution; additive, build unchanged)"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	names="ncurses gettext-minimal"; \
	echo ">> the nano recipe's declared inputs: $$names"; \
	echo ">> ORACLE: Guile's live resolution (specification->package -> #:graft? #f out path)"; \
	oracle=`$(GUIX) repl $(LOAD) tests/resolve-lock.scm $$names 2>/dev/null | grep ' /gnu/store/'`; \
	test -n "$$oracle" || { echo "ERROR: oracle resolution produced nothing" >&2; exit 1; }; \
	printf '%s\n' "$$oracle" | sed 's/^/   oracle: /'; \
	echo ">> td-builder resolves the SAME names from the PINNED lock (no Guile):"; \
	for n in $$names; do \
	  td_path=`"$$tb" resolve "$(CURDIR)/tests/td-build-inputs.lock" "$$n"`; \
	  test -n "$$td_path" || { echo "FAIL: td-builder resolved no path for '$$n'." >&2; exit 1; }; \
	  oracle_path=`printf '%s\n' "$$oracle" | sed -n "s/^$$n //p"`; \
	  test -n "$$oracle_path" || { echo "FAIL: the oracle has no path for '$$n'." >&2; exit 1; }; \
	  echo "   $$n -> td=$$td_path"; \
	  test "$$td_path" = "$$oracle_path" \
	    || { echo "FAIL: td-builder resolved '$$n' to $$td_path but Guile's live resolution is $$oracle_path — the pinned lock is stale or wrong (regenerate on a channel bump: guix repl -L . tests/resolve-lock.scm $$names)." >&2; exit 1; }; \
	done; \
	echo "PASS: td-builder resolved the nano recipe's declared inputs (ncurses + gettext-minimal) from the pinned lock — store-path-equal to Guile's live specification->package resolution, with NO Guile in td's resolution path (additive: the build is unchanged; the resolver is retired last, §5)."
