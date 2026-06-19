# corpus-no-guix — the WHOLE reconstructed corpus builds with td's OWN tooling and NO
# guix/Guile in the build path (DESIGN §7.1 move-off-Guile §5). Consolidates the
# per-recipe build gates onto `td-builder build-recipe`. For each recipe (hello, gzip,
# popt, libatomic-ops, gettext-minimal, nano): ts-eval (boa) lowers recipe-<n>.ts ->
# JSON; `td-builder build-recipe`, run with guix/Guile SCRUBBED FROM PATH, resolves
# every input from the pinned tests/<n>-no-guix.lock (no specification->package),
# assembles the .drv itself (no guix (derivation …)) and realizes it (no guix-daemon).
# Per recipe: STRUCTURAL (built with guix/Guile off PATH — the path needs neither);
# DURABLE behavioral (the artifact runs / ships its lib+header); DURABLE reproducibility
# (`td-builder check` double-builds the .drv, no guix --check); MIGRATION ORACLE
# (distinct store path from guix's build — own, then diverge). The toolchain + locks are
# the guix-built SEED (§5, retired last). Replaces td-build/-deps/-resolved/-phases/
# -corpus/-gettext + td-realize-store/td-loop-build/nano-no-guix and their *-drv.scm.
HEAVY_GATES += corpus-no-guix
corpus-no-guix:
	@echo ">> corpus-no-guix: hello/gzip/popt/libatomic-ops/gettext-minimal/nano all build via td-builder build-recipe (no guix/Guile in the path), run, reproducible (td-builder check), distinct from guix"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$ev" -a -x "$$tb" -a -x "$$node" -a -n "$$tsc" || { echo "ERROR: could not resolve node / tsc / ts-eval / td-builder" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	cu=`grep -- '-coreutils-' "$(CURDIR)/tests/hello-no-guix.lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	. tests/cache-lib.sh; TB="$$tb"; CU="$$cu"; CACHE="$(CURDIR)/.td-build-cache/corpus"; mkdir -p "$$CACHE"; \
	for spec in hello gzip popt libatomic-ops gettext-minimal nano; do \
	  echo "================ $$spec ================"; \
	  lock="$(CURDIR)/tests/$$spec-no-guix.lock"; \
	  test -s "$$lock" || { echo "ERROR: no lock $$lock" >&2; exit 1; }; \
	  grep ' /gnu/store/' "$$lock" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed for $$spec (regenerate locks on a channel bump)" >&2; exit 1; }; \
	  cached_build "$$spec" "$$lock" || exit 1; \
	  if [ -n "$$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — drv unchanged, reused td's prior output (no rebuild): $$out"; else echo "  [STRUCTURAL] built with guix/Guile off PATH: $$out"; fi; \
	  L="$$ns/lib"; \
	  case "$$spec" in \
	    hello) test "`LD_LIBRARY_PATH="$$L" "$$ns/bin/hello"`" = "Hello, world!" || { echo "FAIL: hello did not greet" >&2; exit 1; } ;; \
	    gzip) LD_LIBRARY_PATH="$$L" "$$ns/bin/gzip" --version | grep -q "gzip 1.14" || { echo "FAIL: gzip --version" >&2; exit 1; }; \
	          grep -q "$$out/bin/gzip" "$$ns/bin/gunzip" || { echo "FAIL: gzip's use-absolute-name phase did not apply" >&2; exit 1; } ;; \
	    popt) test -f "$$ns/lib/libpopt.so" -a -f "$$ns/include/popt.h" || { echo "FAIL: popt missing lib/header" >&2; exit 1; } ;; \
	    libatomic-ops) test -f "$$ns/lib/libatomic_ops.a" -a -f "$$ns/include/atomic_ops.h" || { echo "FAIL: libatomic-ops missing lib/header" >&2; exit 1; } ;; \
	    gettext-minimal) LD_LIBRARY_PATH="$$L" "$$ns/bin/msgfmt" --version | grep -q "0.23.1" || { echo "FAIL: gettext msgfmt --version" >&2; exit 1; } ;; \
	    nano) LD_LIBRARY_PATH="$$L" "$$ns/bin/nano" --version | grep -q "version 8.7.1" || { echo "FAIL: nano --version" >&2; exit 1; } ;; \
	  esac; \
	  echo "  [DURABLE behavioral] $$spec runs/ships from td's own store output"; \
	  cached_check "$$spec" || exit 1; \
	  g=`$(GUIX) build "$$spec" 2>/dev/null | grep -v -- '-debug' | head -1 || true`; \
	  if [ -n "$$g" ] && [ "$$out" = "$$g" ]; then echo "FAIL: td's $$spec path equals guix's — expected a distinct own-builder path" >&2; exit 1; fi; \
	  echo "  [MIGRATION ORACLE] distinct from guix's $$spec"; \
	  cached_clean; \
	done; \
	echo "PASS: the whole reconstructed corpus (hello, gzip, popt, libatomic-ops, gettext-minimal, nano) builds via td-builder build-recipe — every input resolved from a pinned lock (no specification->package), the .drv assembled by td (no guix (derivation …)) and realized (no guix-daemon), with guix/Guile SCRUBBED FROM PATH; each artifact runs/ships (durable), is reproducible by td's own double-build (durable), and is at a distinct store path from guix's build (own, then diverge). The toolchain + locks are the guix-built seed (§5, retired last)."
