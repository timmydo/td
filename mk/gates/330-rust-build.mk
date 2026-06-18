# rust-build — td-builder gains its OWN cargo build path (the Guix
# cargo-build-system replacement; move-off-Guile §5). Proven by SELF-HOSTING:
# td's `rust-build` runner (builder/src/build.rs `run_rust`) compiles td-builder
# from %builder-source with `cargo build --offline` — no gnu-build-system and no
# Guix cargo-build-system in the build LOGIC; the rustc/cargo/gcc seed stays
# external (§5, retired last), exactly as the autotools path keeps gcc-toolchain
# external. One source lowers two ways (guix cargo-build-system vs td's runner).
# Per the differential+durable discipline:
#   [STRUCTURAL] the build's builder is the td-builder binary with arg
#     `rust-build` (a native builder — no Guile, no gnu/cargo-build-system).
#   [DURABLE behavioral] the td-built td-builder RUNS (nar-hash) and agrees with
#     the guix-built one — behavioral equivalence, the migration oracle (the two
#     legitimately land at different paths, so equality is behavioral not bytes).
#   [DURABLE repro] td-builder check's double-build agrees the output is
#     reproducible (td's own oracle, not guix build --check).
#   [MIGRATION ORACLE, removable] the td-built path differs from guix's
#     cargo-build-system td-builder (own, then diverge).
# Heavy (a bootstrap td-builder compile + a cargo self-host build + a double-build
# check), so it slots in the heavy pool with the other td gates.
HEAVY_GATES += rust-build
rust-build:
	@echo ">> rust-build: td builds td-builder ITSELF via its own cargo runner (no gnu-build-system / no guix cargo-build-system in the build); it runs, is reproducible, distinct from guix's build"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build the bootstrap td-builder" >&2; exit 1; }; \
	scratch="$(CURDIR)/.rust-build-scratch"; chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	$(GUIX) repl $(LOAD) tests/rust-build-drv.scm 2>/dev/null > "$$scratch/facts.txt"; \
	drv=`sed -n 's/^DRV=//p' "$$scratch/facts.txt"`; \
	out=`sed -n 's/^OUT=//p' "$$scratch/facts.txt"`; \
	gtb=`sed -n 's/^GUIX_TB=//p' "$$scratch/facts.txt"`; \
	test -n "$$drv" -a -n "$$out" -a -n "$$gtb" || { echo "ERROR: could not lower the rust-build drv" >&2; cat "$$scratch/facts.txt" >&2; exit 1; }; \
	echo ">> rust-build .drv: $$drv"; \
	grep -q 'rust-build' "$$drv" || { echo "FAIL: drv builder args lack 'rust-build'" >&2; exit 1; }; \
	grep -q '/bin/td-builder' "$$drv" || { echo "FAIL: drv builder is not the td-builder binary" >&2; exit 1; }; \
	echo "  [STRUCTURAL] the build's builder is the td-builder binary with arg rust-build (native builder — no Guile, no gnu/cargo-build-system)"; \
	built=`$(GUIX) build "$$drv"`; \
	test "$$built" = "$$out" || { echo "FAIL: built path ($$built) != lowered OUT ($$out)" >&2; exit 1; }; \
	test -x "$$out/bin/td-builder" || { echo "FAIL: rust-build produced no td-builder binary at $$out/bin/td-builder" >&2; exit 1; }; \
	echo "  built (td's cargo runner): $$out"; \
	printf 'td rust-build behavioral probe\n' > "$$scratch/probe"; \
	h_td=`"$$out/bin/td-builder" nar-hash "$$scratch/probe"`; \
	h_gx=`"$$tb" nar-hash "$$scratch/probe"`; \
	test -n "$$h_td" || { echo "FAIL: the td-built td-builder did not run / produced no nar-hash" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built td-builder RUNS: nar-hash = $$h_td"; \
	test "$$h_td" = "$$h_gx" || { echo "FAIL: td-built and guix-built td-builder disagree ($$h_td != $$h_gx)" >&2; exit 1; }; \
	echo "  [DURABLE behavioral / migration oracle] it agrees with the guix-built td-builder (behavioral equivalence)"; \
	{ sed -n 's/^INPUT=//p' "$$scratch/facts.txt"; echo "$$drv"; } | xargs $(GUIX) gc -R | sort -u > "$$scratch/paths.txt"; \
	echo "   staged closure: $$(wc -l < "$$scratch/paths.txt") store items"; \
	"$$tb" check "$$drv" "$$scratch/paths.txt" "$$scratch/chk" > "$$scratch/checkout.txt" 2>"$$scratch/chk.err" \
	  || { echo "FAIL: rust-build NOT reproducible (td-builder check):" >&2; cat "$$scratch/checkout.txt" "$$scratch/chk.err" >&2; exit 1; }; \
	grep -qE "^CHECK out $$out sha256:[0-9a-f]+ reproducible$$" "$$scratch/checkout.txt" \
	  || { echo "FAIL: td-builder check did not confirm $$out reproducible:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	echo "  [DURABLE repro] td-builder check double-build agrees the rust-build output is reproducible"; \
	if [ "$$out" = "$$gtb" ]; then echo "FAIL: td's rust-build path equals guix's cargo-build-system path — expected a distinct own-builder path" >&2; exit 1; fi; \
	echo "  [MIGRATION ORACLE, removable] distinct from guix's cargo-build-system td-builder ($$gtb)"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; \
	echo "PASS: td built td-builder ITSELF via its own rust-build cargo runner — Guile / gnu-build-system / guix cargo-build-system OUT of the build logic (rustc/cargo/gcc seed external, §5); the output RUNS and agrees with the guix-built builder (durable behavioral), is reproducible by td's own double-build (durable), and lands at a distinct store path from guix's cargo-build-system build (own, then diverge)."
