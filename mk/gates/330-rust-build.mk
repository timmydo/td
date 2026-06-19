# rust-build — td-builder self-hosts through its OWN build path, end to end (the
# Guix cargo-build-system replacement; move-off-Guile §5). td builds td-builder
# ITSELF via `td-builder build-recipe` on a `buildSystem: "rust"` recipe
# (tests/ts/recipe-td-builder.ts, authored in TS, emitted Guile-free by ts-eval):
# every input is resolved from a pinned lock (tests/td-builder-rust.lock, no
# specification->package), the `.drv` is ASSEMBLED by td (store::assemble_drv — no
# guix (derivation …)), and it is REALIZED daemon-free (realize_drv — no
# guix-daemon). The whole BUILD runs with guix/Guile SCRUBBED FROM PATH. So nothing
# in td-builder's own build path is guix/Guile — only the rustc/cargo/gcc seed and
# the lock stay external (§5, retired LAST), exactly as the toolchain-no-guix gate.
# This routes the self-host onto the same build-recipe rail the corpus/toolchain
# leaves and nano use (own-builder-daemon, #74), replacing the earlier Guile
# `(derivation …)` lowering.
#
# The source is the LIVE builder/ tree (it changes every edit), so the gate interns
# the CURRENT tree (guix builds the exported %builder-source — seed prep, not a
# specification->package) and appends it to the committed seed lock.
# Per the differential+durable discipline:
#   [STRUCTURAL] the build runs with guix/Guile off PATH and produces td-builder.
#   [DURABLE behavioral] the td-built td-builder RUNS (nar-hash) and agrees with the
#     guix-built one (behavioral equivalence — the migration-oracle leg).
#   [DURABLE repro] td-builder check's double-build agrees the output is
#     reproducible (td's own oracle, not guix build --check).
#   [MIGRATION ORACLE, removable] the td-built path differs from guix's
#     cargo-build-system td-builder (own, then diverge).
# Heavy (a bootstrap td-builder compile + a cargo self-host build + a double-build
# check), so it slots in the heavy pool with the other td gates.
HEAVY_GATES += rust-build
# Ordered AFTER the parallel build phase (its cargo self-host build would otherwise use
# all cores concurrently with build-recipes' fan-out). td-builder is NOT in BUILD_SPECS
# — its lock is extended with the freshly-interned source, so it stays self-contained.
BUILD_GATES += rust-build
rust-build:
	@echo ">> rust-build: td self-hosts td-builder via build-recipe (buildSystem rust) — .drv assembled + realized by td, guix/Guile off PATH; it runs, is reproducible, distinct from guix's build"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	evdrv=`$(GUIX) repl $(LOAD) tests/ts-eval-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	ev=`$(GUIX) build "$$evdrv"`/bin/td-ts-eval; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$ev" -a -x "$$tb" -a -x "$$node" -a -n "$$tsc" || { echo "ERROR: could not resolve node / tsc / ts-eval / td-builder" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	lock0="$(CURDIR)/tests/td-builder-rust.lock"; \
	test -s "$$lock0" || { echo "ERROR: no lock $$lock0" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-' "$$lock0" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	scratch="$(CURDIR)/.td-build-cache/rust-build"; mkdir -p "$$scratch/tmp" "$$scratch/b"; rm -f "$$scratch/b/"*.drv; \
	grep ' /gnu/store/' "$$lock0" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the rust seed (regenerate the lock on a channel bump)" >&2; exit 1; }; \
	src=`$(GUIX) repl $(LOAD) tests/td-builder-source.scm 2>/dev/null | sed -n 's/^SRC=//p'`; \
	test -n "$$src" -a -d "$$src" || { echo "ERROR: could not intern the current builder tree (%builder-source)" >&2; exit 1; }; \
	echo ">> interned the CURRENT builder tree: $$src"; \
	lock="$$scratch/td-builder-rust.lock"; { cat "$$lock0"; echo "td-builder-source $$src"; } > "$$lock"; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-td-builder.ts" > "$$scratch/td-builder.json"; \
	test -s "$$scratch/td-builder.json" || { echo "ERROR: ts-emit produced no JSON for td-builder" >&2; exit 1; }; \
	grep -q '"buildSystem":"rust"' "$$scratch/td-builder.json" || { echo "FAIL: recipe JSON is not buildSystem rust" >&2; cat "$$scratch/td-builder.json" >&2; exit 1; }; \
	sd="$$scratch/b"; mkdir -p "$$sd"; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" "$$tb" build-recipe "$$scratch/td-builder.json" "$$lock" "$$sd" /var/guix/db/db.sqlite > "$$scratch/bout" 2>"$$scratch/err" || { echo "FAIL: build-recipe self-host (guix/Guile off PATH):" >&2; tail -30 "$$scratch/err" >&2; exit 1; }; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	if grep -qx 'CACHE=hit' "$$scratch/bout"; then hit=1; else hit=; grep -q 'no guix (derivation), no Guile' "$$scratch/err" || { echo "FAIL: build-recipe did not assemble the .drv itself" >&2; cat "$$scratch/err" >&2; exit 1; }; fi; \
	ns="$$sd/newstore/`basename "$$out"`"; \
	test -x "$$ns/bin/td-builder" || { echo "FAIL: self-host produced no td-builder binary at $$ns/bin/td-builder" >&2; exit 1; }; \
	if [ -n "$$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — builder source unchanged, reused td's prior self-host build (no rebuild): $$out"; else echo "  [STRUCTURAL] td assembled + realized the .drv with guix/Guile off PATH: $$out"; fi; \
	printf 'td rust-build behavioral probe\n' > "$$scratch/probe"; \
	h_td=`"$$ns/bin/td-builder" nar-hash "$$scratch/probe"`; \
	h_gx=`"$$tb" nar-hash "$$scratch/probe"`; \
	test -n "$$h_td" || { echo "FAIL: the td-built td-builder did not run / produced no nar-hash" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built td-builder RUNS: nar-hash = $$h_td"; \
	test "$$h_td" = "$$h_gx" || { echo "FAIL: td-built and guix-built td-builder disagree ($$h_td != $$h_gx)" >&2; exit 1; }; \
	echo "  [DURABLE behavioral / migration oracle] it agrees with the guix-built td-builder (behavioral equivalence)"; \
	if [ -n "$$hit" ] && [ -f "$$sd/verified-reproducible" ]; then \
	  echo "  [DURABLE repro] CACHED: builder source unchanged + previously verified reproducible — td-builder check skipped (verdict memoized)"; \
	else \
	  rm -rf "$$scratch/chk"; "$$tb" check "$$sd"/*.drv "$$sd/closure.txt" "$$scratch/chk" > "$$scratch/checkout.txt" 2>"$$scratch/chk.err" \
	    || { echo "FAIL: rust-build NOT reproducible (td-builder check):" >&2; cat "$$scratch/checkout.txt" "$$scratch/chk.err" >&2; exit 1; }; \
	  grep -qE "^CHECK out $$out sha256:[0-9a-f]+ reproducible$$" "$$scratch/checkout.txt" \
	    || { echo "FAIL: td-builder check did not confirm $$out reproducible:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	  : > "$$sd/verified-reproducible"; \
	  echo "  [DURABLE repro] td-builder check double-build agrees the rust-build output is reproducible"; \
	fi; \
	gtb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`; \
	if [ "$$out" = "$$gtb" ]; then echo "FAIL: td's rust-build path equals guix's cargo-build-system path — expected a distinct own-builder path" >&2; exit 1; fi; \
	echo "  [MIGRATION ORACLE, removable] distinct from guix's cargo-build-system td-builder ($$gtb)"; \
	rm -rf "$$scratch/chk" "$$scratch/tmp" "$$scratch/bout" "$$scratch/err" "$$scratch/checkout.txt" "$$scratch/chk.err"; mkdir -p "$$scratch/tmp"; \
	echo "PASS: td self-hosted td-builder via build-recipe (buildSystem rust) — every input resolved from a pinned lock (no specification->package), the .drv assembled by td (no guix (derivation …)) and realized daemon-free (no guix-daemon), with guix/Guile SCRUBBED FROM PATH; the output RUNS and agrees with the guix-built builder (durable behavioral), is reproducible by td's own double-build (durable), and lands at a distinct store path from guix's cargo-build-system build (own, then diverge). The rustc/cargo/gcc seed stays external (§5, retired last)."
