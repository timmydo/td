# corpus-no-guix — the WHOLE reconstructed corpus builds with td's OWN tooling and NO
# guix/Guile in the build path (DESIGN §7.1 move-off-Guile §5). Consolidates the
# per-recipe build gates onto `td-builder build-recipe`. For each recipe (hello, gzip,
# popt, libatomic-ops, gettext-minimal, nano, which, gperf): ts-eval (boa) lowers recipe-<n>.ts
# -> JSON; `td-builder build-recipe`, run with guix/Guile SCRUBBED FROM PATH, resolves
# every input from the pinned tests/<n>-no-guix.lock (no specification->package),
# assembles the .drv itself (no guix (derivation …)) and realizes it (no guix-daemon).
# Per recipe: STRUCTURAL (built with guix/Guile off PATH — the path needs neither);
# DURABLE behavioral (the artifact runs / ships its lib+header); DURABLE reproducibility
# (`td-builder check` double-builds the .drv, no guix --check); DURABLE self-discrimination
# (a perturbed recipe-<n>-perturbed.ts — a load-bearing field change — assembles a DISTINCT
# .drv, so the build is recipe-driven, not vacuous); MIGRATION ORACLE (distinct store path
# from guix's build — own, then diverge; the removable Guix leg). The toolchain + locks are
# the guix-built SEED (§5, retired last). Replaces td-build/-deps/-resolved/-phases/
# -corpus/-gettext + td-realize-store/td-loop-build/nano-no-guix and their *-drv.scm.
HEAVY_GATES += corpus-no-guix
# Built up front by the parallel `build-recipes` phase (into the shared cache); this
# gate then cache-hits + memo-skips and only asserts behavior/oracle.
corpus_SPECS := hello gzip popt libatomic-ops gettext-minimal nano which gperf
# Specs that carry the DURABLE self-discrimination leg below (perturbed recipe ->
# distinct .drv). Only specs whose `recipe-<n>-perturbed.ts` perturbs a load-bearing
# RECIPE FIELD (e.g. configureFlags) belong here: in the build-recipe path the SOURCE
# is resolved from the pinned lock, not the recipe, so the older specs whose perturbed
# recipe only flips a SOURCE-HASH byte (gzip/popt/libatomic-ops/nano/gettext-minimal)
# are vacuous HERE — their source-hash discriminator is load-bearing only in their own
# guix-differential gates, not in this no-guix build path.
corpus_SELFDISC_SPECS := which gperf
BUILD_SPECS  += $(corpus_SPECS)
BUILD_GATES  += corpus-no-guix
corpus-no-guix:
	@echo ">> corpus-no-guix: hello/gzip/popt/libatomic-ops/gettext-minimal/nano/which/gperf all build via td-builder build-recipe (no guix/Guile in the path), run, reproducible (td-builder check), distinct from guix"
	@set -euo pipefail; \
	tgz=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-tsgo-tarball)'`; tsgo=`sh tests/tsgo.sh "$$tgz"`; \
	test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: could not resolve td-tsgo" >&2; exit 1; }; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts"; \
	cu=`grep -- '-coreutils-' "$(CURDIR)/tests/hello-no-guix.lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; CU="$$cu"; CACHE="$(CURDIR)/.td-build-cache/pkg"; mkdir -p "$$CACHE"; \
	case "$$TD_TS_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_TS_EVAL is not td's own build ($$TD_TS_EVAL)" >&2; exit 1 ;; esac; \
	echo "  [DURABLE structural] recipes evaluate with td's OWN td-ts-eval ($$TD_TS_EVAL) — not the guix-built one (brick 4b)"; \
	for spec in $(corpus_SPECS); do \
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
	    which) LD_LIBRARY_PATH="$$L" "$$ns/bin/which" --version | grep -q "which v2.21" || { echo "FAIL: which --version" >&2; exit 1; }; \
	           test "`LD_LIBRARY_PATH="$$L" PATH="$$CU/bin" "$$ns/bin/which" ls`" = "$$CU/bin/ls" || { echo "FAIL: which did not locate ls on PATH" >&2; exit 1; } ;; \
	    gperf) LD_LIBRARY_PATH="$$L" "$$ns/bin/gperf" --version | grep -q "GNU gperf 3.3" || { echo "FAIL: gperf --version" >&2; exit 1; }; \
	           printf '%%%%\nfoo\nbar\n%%%%\n' | LD_LIBRARY_PATH="$$L" "$$ns/bin/gperf" | grep -q "in_word_set" || { echo "FAIL: gperf did not generate a hash lookup" >&2; exit 1; } ;; \
	  esac; \
	  echo "  [DURABLE behavioral] $$spec runs/ships from td's own store output"; \
	  cached_check "$$spec" || exit 1; \
	  case " $(corpus_SELFDISC_SPECS) " in *" $$spec "*) selfdisc=1 ;; *) selfdisc= ;; esac; \
	  if [ -n "$$selfdisc" ]; then \
	    rdrv=`grep -hoE '/gnu/store/[a-z0-9]+-'"$$spec"'-[^ ]+\.drv' "$$sd/err" "$$sd/bout" 2>/dev/null | head -1`; \
	    test -n "$$rdrv" || { echo "FAIL: could not read the real $$spec .drv store path (self-discrimination leg)" >&2; exit 1; }; \
	    pdir="$$sd/perturbed"; rm -rf "$$pdir"; mkdir -p "$$pdir/b" "$$pdir/tmp"; \
	    sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-$$spec-perturbed.ts" > "$$pdir/recipe.json" || { echo "FAIL: ts-emit $$spec-perturbed" >&2; exit 1; }; \
	    : "$${TB:?}"; \
	    env -i HOME="$$pdir" TMPDIR="$$pdir/tmp" PATH="$$CU/bin" \
	      TD_BUILDER_PATH="$$TD_BUILDER_PATH" TD_BUILDER_STORE="$$TD_BUILDER_STORE" TD_BUILDER_DB="$$TD_BUILDER_DB" \
	      "$$TB" build-recipe "$$pdir/recipe.json" "$$lock" "$$pdir/b" /var/guix/db/db.sqlite > "$$pdir/out" 2>&1 || true; \
	    pdrv=`grep -hoE '/gnu/store/[a-z0-9]+-'"$$spec"'-[^ ]+\.drv' "$$pdir/out" 2>/dev/null | head -1`; \
	    test -n "$$pdrv" || { echo "FAIL: perturbed $$spec recipe did not assemble a .drv (self-discrimination leg)" >&2; tail -5 "$$pdir/out" >&2; exit 1; }; \
	    test "$$pdrv" != "$$rdrv" || { echo "FAIL: perturbed $$spec recipe assembled the SAME .drv ($$rdrv) — the recipe's content is not load-bearing in the build (self-discrimination vacuous)" >&2; exit 1; }; \
	    echo "  [DURABLE self-discrimination] perturbed $$spec recipe -> distinct .drv (real $$rdrv vs perturbed $$pdrv); the recipe's content is load-bearing"; \
	    rm -rf "$$pdir"; \
	  fi; \
	  g=`$(GUIX) build "$$spec" 2>/dev/null | grep -v -- '-debug' | head -1 || true`; \
	  if [ -n "$$g" ] && [ "$$out" = "$$g" ]; then echo "FAIL: td's $$spec path equals guix's — expected a distinct own-builder path" >&2; exit 1; fi; \
	  echo "  [MIGRATION ORACLE] distinct from guix's $$spec"; \
	  cached_clean; \
	done; \
	echo "PASS: the whole reconstructed corpus (hello, gzip, popt, libatomic-ops, gettext-minimal, nano, which, gperf) builds via td-builder build-recipe — every input resolved from a pinned lock (no specification->package), the .drv assembled by td (no guix (derivation …)) and realized (no guix-daemon), with guix/Guile SCRUBBED FROM PATH; each artifact runs/ships (durable), is reproducible by td's own double-build (durable), and is at a distinct store path from guix's build (own, then diverge). The toolchain + locks are the guix-built seed (§5, retired last)."
