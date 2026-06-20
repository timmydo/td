# cmake — td builds a cmake-based package from source via its OWN builder (the Guix
# cmake-build-system replacement; move-off-Guile §5). td's run_cmake phase runner
# (builder/src/build.rs) configures a cmake project OUT OF SOURCE, builds it, and
# installs it — with NO gnu-build-system and NO guix/Guile in the build path. The
# demonstrator is a trivial in-tree cmake C project (tests/cmake-demo: a CMakeLists
# building the `td-cmake-hello` binary), authored as a TS recipe
# (tests/ts/recipe-td-cmake-demo.ts, buildSystem "cmake", emitted Guile-free by
# ts-eval). `td-builder build-recipe` resolves every input from the pinned lock
# (tests/td-cmake-demo.lock, no specification->package), routes buildSystem "cmake" to
# the cmake-build phase runner, ASSEMBLES the .drv itself (store::assemble_drv — no
# guix (derivation …)), and REALIZES it daemon-free (realize_drv — no guix-daemon).
# cmake/gcc/make are the external SEED (§5, retired LAST), exactly as the autotools
# (gnu) path uses make/gcc.
#
# The source is the LIVE tests/cmake-demo tree, so the gate interns the CURRENT tree
# with td's OWN recursive addToStore (tests/intern-src.sh → store-add-recursive, the
# gate-285 primitive) into a td-owned store dir + db — NO `guix repl … lower-object`
# daemon interning (move-off-Guile §5) — and appends the content-addressed path to the
# committed seed lock. build-recipe is handed that store dir + db.
#
# Per the differential + durable discipline:
#   [STRUCTURAL] the build runs with guix/Guile off PATH and produces the binary, AND
#     the .drv selected the cmake phase runner (`arg cmake-build`).
#   [DURABLE behavioral] the cmake-built binary RUNS and prints "td cmake-build hello".
#   [DURABLE repro] td-builder check's double-build agrees the output is reproducible
#     (td's own oracle, not guix build --check).
#   [MIGRATION ORACLE, removable] the SAME demonstrator lowered through guix's
#     cmake-build-system lands at a DISTINCT store path (own, then diverge). Computed
#     path-only (guix build -d, no heavy realization); retiring guix deletes this leg.
# Heavy (a cmake configure + a C build + a double-build check), so it slots in the
# heavy pool with the other own-builder gates.
HEAVY_GATES += cmake
# Ordered AFTER the parallel build phase (its cmake build would otherwise oversubscribe
# cores against build-recipes' fan-out). Not in BUILD_SPECS — the source is interned at
# gate time by td's OWN recursive addToStore (no `guix repl`), so it is self-contained.
BUILD_GATES += cmake
cmake:
	@echo ">> cmake: td builds td-cmake-demo (a cmake C project) via build-recipe (buildSystem cmake) — .drv assembled + realized by td, guix/Guile off PATH; it runs, is reproducible, distinct from guix's cmake-build-system"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	ev=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-ts-eval)'`/bin/td-ts-eval; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$ev" -a -x "$$tb" -a -x "$$node" -a -n "$$tsc" || { echo "ERROR: could not resolve node / tsc / ts-eval / td-builder" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	lock0="$(CURDIR)/tests/td-cmake-demo.lock"; \
	test -s "$$lock0" || { echo "ERROR: no lock $$lock0" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-' "$$lock0" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	scratch="$(CURDIR)/.td-build-cache/cmake"; mkdir -p "$$scratch/tmp" "$$scratch/b"; rm -f "$$scratch/b/"*.drv; \
	grep ' /gnu/store/' "$$lock0" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the cmake seed (regenerate the lock on a channel bump)" >&2; exit 1; }; \
	srcinfo=`sh tests/intern-src.sh "$$tb" td-cmake-demo-src "$(CURDIR)/tests/cmake-demo" "$$scratch"` || { echo "ERROR: td could not intern the cmake-demo tree (store-add-recursive)" >&2; exit 1; }; \
	eval "$$srcinfo"; \
	test -n "$$src" -a -d "$$srcstore/`basename "$$src"`" || { echo "ERROR: td interned no cmake-demo source tree (store-add-recursive)" >&2; exit 1; }; \
	echo ">> td interned the CURRENT cmake-demo tree (recursive addToStore, no guix repl / no daemon): $$src"; \
	lock="$$scratch/td-cmake-demo.lock"; { cat "$$lock0"; echo "td-cmake-demo-source $$src"; } > "$$lock"; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-td-cmake-demo.ts" > "$$scratch/td-cmake-demo.json"; \
	test -s "$$scratch/td-cmake-demo.json" || { echo "ERROR: ts-emit produced no JSON for td-cmake-demo" >&2; exit 1; }; \
	grep -q '"buildSystem":"cmake"' "$$scratch/td-cmake-demo.json" || { echo "FAIL: recipe JSON is not buildSystem cmake" >&2; cat "$$scratch/td-cmake-demo.json" >&2; exit 1; }; \
	sd="$$scratch/b"; mkdir -p "$$sd"; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" "$$tb" build-recipe "$$scratch/td-cmake-demo.json" "$$lock" "$$sd" /var/guix/db/db.sqlite "$$srcstore" "$$srcdb" > "$$scratch/bout" 2>"$$scratch/err" || { echo "FAIL: build-recipe cmake build (guix/Guile off PATH):" >&2; tail -30 "$$scratch/err" >&2; exit 1; }; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	if grep -qx 'CACHE=hit' "$$scratch/bout"; then hit=1; else hit=; grep -q 'no guix (derivation), no Guile' "$$scratch/err" || { echo "FAIL: build-recipe did not assemble the .drv itself" >&2; cat "$$scratch/err" >&2; exit 1; }; fi; \
	grep -q 'arg cmake-build' "$$sd"/*.drv || { echo "FAIL: the .drv did not select the cmake-build phase runner" >&2; exit 1; }; \
	ns="$$sd/newstore/`basename "$$out"`"; \
	test -x "$$ns/bin/td-cmake-hello" || { echo "FAIL: cmake build produced no binary at $$ns/bin/td-cmake-hello" >&2; exit 1; }; \
	if [ -n "$$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — cmake-demo source unchanged, reused td's prior build (no rebuild): $$out"; else echo "  [STRUCTURAL] td assembled + realized the .drv (arg cmake-build) with guix/Guile off PATH: $$out"; fi; \
	got=`"$$ns/bin/td-cmake-hello"`; \
	test "$$got" = "td cmake-build hello" || { echo "FAIL: td-cmake-hello printed '$$got', expected 'td cmake-build hello'" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the cmake-built binary RUNS and prints '$$got'"; \
	if [ -n "$$hit" ] && [ -f "$$sd/verified-reproducible" ]; then \
	  echo "  [DURABLE repro] CACHED: cmake-demo source unchanged + previously verified reproducible — td-builder check skipped (verdict memoized)"; \
	else \
	  rm -rf "$$scratch/chk"; "$$tb" check "$$sd"/*.drv "$$sd/closure.txt" "$$scratch/chk" > "$$scratch/checkout.txt" 2>"$$scratch/chk.err" \
	    || { echo "FAIL: cmake build NOT reproducible (td-builder check):" >&2; tail -6 "$$scratch/checkout.txt" "$$scratch/chk.err" >&2; exit 1; }; \
	  grep -qE "^CHECK out $$out sha256:[0-9a-f]+ reproducible$$" "$$scratch/checkout.txt" \
	    || { echo "FAIL: td-builder check did not confirm $$out reproducible:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	  : > "$$sd/verified-reproducible"; \
	  echo "  [DURABLE repro] td-builder check double-build agrees the cmake build is reproducible"; \
	fi; \
	oracle="$$scratch/oracle.scm"; \
	{ echo "(use-modules (guix packages) (guix gexp) (guix build-system cmake) ((guix licenses) #:prefix license:))"; \
	  echo "(package (name \"td-cmake-demo-guix\") (version \"0.1.0\")"; \
	  echo "  (source (local-file \"$(CURDIR)/tests/cmake-demo\" \"td-cmake-demo-src\" #:recursive? #t))"; \
	  echo "  (build-system cmake-build-system) (arguments (list #:tests? #f))"; \
	  echo "  (synopsis \"o\") (description \"cmake-build-system oracle.\") (home-page \"https://example.invalid\") (license license:gpl3+))"; } > "$$oracle"; \
	gdrv=`$(GUIX) build -d -f "$$oracle" 2>/dev/null` || { echo "ERROR: could not compute the guix cmake-build-system oracle derivation (warm its closure on a channel bump)" >&2; exit 1; }; \
	gout=`printf '(use-modules (guix derivations))\n(for-each (lambda (o) (display (derivation-output-path (cdr o))) (newline)) (derivation-outputs (read-derivation-from-file "%s")))\n' "$$gdrv" | $(GUIX) repl 2>/dev/null | head -1`; \
	test -n "$$gout" || { echo "ERROR: could not read the guix oracle output path from $$gdrv" >&2; exit 1; }; \
	if [ "$$out" = "$$gout" ]; then echo "FAIL: td's cmake-build path equals guix's cmake-build-system path — expected a distinct own-builder path" >&2; exit 1; fi; \
	echo "  [MIGRATION ORACLE, removable] distinct from guix's cmake-build-system build ($$gout)"; \
	rm -rf "$$scratch/chk" "$$scratch/tmp" "$$scratch/bout" "$$scratch/err" "$$scratch/checkout.txt" "$$scratch/chk.err" "$$oracle"; mkdir -p "$$scratch/tmp"; \
	echo "PASS: td built td-cmake-demo (a cmake C project) via td-builder build-recipe — every input resolved from a pinned lock (no specification->package), buildSystem \"cmake\" routed to td's own cmake phase runner, the .drv assembled by td (no guix (derivation …)) and realized daemon-free (no guix-daemon), with guix/Guile SCRUBBED FROM PATH; the binary runs (durable behavioral), is reproducible by td's own double-build (durable), and lands at a distinct store path from guix's cmake-build-system build (own, then diverge). cmake/gcc/make stay external (§5, retired last)."
