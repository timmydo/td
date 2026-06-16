# td-build-corpus — route the reconstructed corpus recipes through td's OWN builder
# (DESIGN §7.1 move-off-Guile §5; the step that makes td-recipe.scm replaceable).
# The corpus-* gates lower these recipes through gnu-build-system / td-recipe (proven
# byte-identical to Guix). This builds the SAME recipes through system/td-build —
# builder = the td-builder Rust binary, the recipe's configure-flags + phases run by
# td's OWN phase runner (no gnu-build-system, no Guile in the build). The own-builder
# output is a DISTINCT store path, so each is proven BEHAVIORALLY + structurally +
# reproducibly, not by NAR-equality. This covers the single-output recipes that build
# under td's minimal phase set today — popt (a custom substitute*/which phase) and
# libatomic-ops (multi-output recipe → td builds a single `out`); gzip is the sibling
# `td-build-phases` gate. (pkg-config's bundled glib hits a C-standard wall and
# gettext-minimal needs more standard phases — deferred gnu-build-system fidelity.)
# Per package: STRUCTURAL (builder = td-builder) + DURABLE behavioral (the artifact
# ships its library/header) + DURABLE reproducibility (td-builder check double-build)
# + INDEPENDENCE (distinct path from the corpus build).
HEAVY_GATES += td-build-corpus
td-build-corpus:
	@echo ">> td-build-corpus: the reconstructed recipes popt + libatomic-ops built by td's OWN Rust builder (configure-flags + phases in Rust, no gnu-build-system); behavioral + reproducible + distinct from the corpus (move-off-Guile §5: routing recipes off td-recipe.scm)"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$evdrv" || { echo "ERROR: could not lower the td-ts-eval derivation" >&2; exit 1; }; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -n "$$node" -a -n "$$tsc" -a -x "$$ev" -a -x "$$tb" || { echo "ERROR: could not resolve node / td-typescript / td-ts-eval / td-builder" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	for spec in "popt:recipe-popt.ts" "libatomic-ops:recipe-libatomic-ops.ts"; do \
	  name="$${spec%%:*}"; recipe="$${spec#*:}"; \
	  echo "================ $$name via td's OWN builder ================"; \
	  rj=`sh tests/ts-emit.sh "$(CURDIR)/tests/ts/$$recipe"`; \
	  test -n "$$rj" || { echo "ERROR: ts-emit produced no JSON for $$recipe" >&2; exit 1; }; \
	  vars=`TD_RECIPE_JSON="$$rj" $(GUIX) repl $(LOAD) tests/td-build-recipe-drv.scm 2>/dev/null`; \
	  td_drv=`printf '%s\n' "$$vars" | sed -n 's/^TD_DRV=//p'`; \
	  td_out=`printf '%s\n' "$$vars" | sed -n 's/^TD_OUT=//p'`; \
	  td_builder=`printf '%s\n' "$$vars" | sed -n 's/^TD_BUILDER=//p'`; \
	  test -n "$$td_drv" -a -n "$$td_out" -a -n "$$td_builder" || { echo "ERROR: could not lower $$name via td-build" >&2; exit 1; }; \
	  echo ">> td-build drv : $$td_drv"; \
	  echo ">> [STRUCTURAL] builder is td's Rust binary, not gnu-build-system's guile: $$td_builder"; \
	  case "$$td_builder" in td-builder) : ;; *) echo "FAIL: $$name builder is '$$td_builder', expected td-builder." >&2; exit 1;; esac; \
	  echo ">> build $$name with td's OWN builder"; \
	  out=`$(GUIX) build "$$td_drv"`; \
	  test -n "$$out" -a "$$out" = "$$td_out" || { echo "FAIL: $$name td build produced no/incorrect out ($$out vs $$td_out)" >&2; exit 1; }; \
	  echo ">> [DURABLE: behavioral] the built $$name ships its library/header — no Guix oracle"; \
	  case "$$name" in \
	    popt) test -f "$$out/lib/libpopt.so" -a -f "$$out/include/popt.h" \
	            || { echo "FAIL: td-built popt missing lib/libpopt.so or include/popt.h." >&2; exit 1; }; \
	          echo "   lib/libpopt.so + include/popt.h present" ;; \
	    libatomic-ops) test -f "$$out/lib/libatomic_ops.a" -a -f "$$out/include/atomic_ops.h" \
	            || { echo "FAIL: td-built libatomic-ops missing lib/libatomic_ops.a or include/atomic_ops.h." >&2; exit 1; }; \
	          echo "   lib/libatomic_ops.a + include/atomic_ops.h present" ;; \
	    *) echo "FAIL: no behavioral check for $$name" >&2; exit 1 ;; \
	  esac; \
	  echo ">> [DURABLE: reproducibility] td-builder check double-builds $$name's .drv (no guix build --check)"; \
	  printf '%s\n' "$$vars" | sed -n 's/^TD_IN=//p' > "$(CURDIR)/.tdck-corpus.in"; \
	  TD_GUIX="$(GUIX)" sh tests/td-check-repro.sh "$$tb" "$$td_drv" "$(CURDIR)/.tdck-corpus.in" "$(CURDIR)/.tdck-corpus"; \
	  rm -f "$(CURDIR)/.tdck-corpus.in"; \
	  echo ">> [INDEPENDENCE] distinct from the corpus $$name (own builder → own path)"; \
	  corpus_out=`$(GUIX) build "$$name" 2>/dev/null | grep -- "-$$name-" | head -n1 || true`; \
	  if [ -n "$$corpus_out" ] && [ "$$out" = "$$corpus_out" ]; then echo "FAIL: td-built $$name path equals the corpus path." >&2; exit 1; fi; \
	  echo "   td-built $$name: $$out (distinct from corpus)"; \
	done; \
	echo "PASS: td's OWN Rust builder built popt + libatomic-ops from the reconstructed recipes (configure-flags + phases applied in Rust, no gnu-build-system) — each ships its library/header (durable behavioral), is reproducible by td's own double-build, and lands at a distinct store path from the corpus build. The own-builder path now covers these recipes — td-recipe.scm is replaceable for them."
