# rust-coreutils — td builds the FULL uutils coreutils MULTICALL binary from source
# (the Rust-focused minimal distro, step 1; move-off-Guile §5). The published
# `coreutils` crate 0.9.0 (default-run "coreutils", default feature feat_common_core)
# is built via `td-builder build-recipe` (buildSystem "rust", recipe-uutils.ts) with
# its FULL dependency closure — 507 crates (all uu_* members + shared deps) — vendored:
# each is a fixed-output fetch from static.crates.io (sha256 == its Cargo.lock
# checksum), pinned in tests/uutils-coreutils.lock; the source is the upstream
# `coreutils` crate tarball, also lock-supplied (keyed `uutils-source`). run_rust
# assembles the cargo vendor dir and builds `cargo --release --offline --frozen`; the
# .drv is assembled by td (no guix (derivation …)) and realized daemon-free (no
# guix-daemon), with guix/Guile SCRUBBED FROM PATH. No builder code change — the
# tarball-source + vendored-deps path (uu_cat / russh) already covers it. The
# rustc/cargo/gcc seed + locked deps are the external SEED (§5, retired last).
#
# ALL-DURABLE (no guix oracle): there is no guix build of the uutils multicall to diff
# against — every leg stands with no Guix in the room:
#   [STRUCTURAL] the build runs guix/Guile off PATH, produces the `coreutils` binary,
#     and the .drv carries TD_VENDOR_CRATES + the td-bootstrapped stage0 as builder.
#   [DURABLE behavioral] the ONE multicall binary actually dispatches to many tools —
#     mkdir/cp/cat/ls/mv/rm round-trip through `coreutils <util>` (real coreutils
#     behavior across the multicall, not just --version).
#   [DURABLE repro] td-builder check's double-build agrees the build is reproducible
#     across the whole 507-crate graph (proc-macros, build scripts, fluent locales).
HEAVY_GATES += rust-coreutils
# Ordered AFTER the parallel build-recipes phase (its 507-crate cargo build would
# otherwise oversubscribe cores against build-recipes' fan-out). Not in BUILD_SPECS —
# its lock (tests/uutils-coreutils.lock) is self-contained, so the gate builds it itself.
BUILD_GATES += rust-coreutils
rust-coreutils:
	@echo ">> rust-coreutils: td builds the FULL uutils coreutils multicall (coreutils 0.9.0, 507 vendored deps) from source via build-recipe (offline, guix/Guile off PATH); the multicall dispatches mkdir/cp/cat/ls/mv/rm + is reproducible"
	@set -euo pipefail; \
	tgz=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-tsgo-tarball)'`; tsgo=`sh tests/tsgo.sh "$$tgz"`; \
	test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: could not resolve td-tsgo" >&2; exit 1; }; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts"; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; tb="$$TB"; \
	case "$$TD_TS_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_TS_EVAL is not td's own build ($$TD_TS_EVAL)" >&2; exit 1 ;; esac; \
	echo "  [DURABLE structural] ts-emit evaluates with td's OWN td-ts-eval ($$TD_TS_EVAL) — not the guix-built one (brick 4c)"; \
	lock="$(CURDIR)/tests/uutils-coreutils.lock"; \
	test -s "$$lock" || { echo "ERROR: no lock $$lock" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-9' "$$lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils seed in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	ncrate=`grep -cE '\.crate /gnu/store/' "$$lock"`; \
	test "$$ncrate" -ge 400 || { echo "ERROR: lock has <400 vendored .crate deps ($$ncrate) — regenerate from coreutils' Cargo.lock" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-build-cache/rust-coreutils"; mkdir -p "$$scratch/tmp" "$$scratch/b"; rm -f "$$scratch/b/"*.drv; \
	grep ' /gnu/store/' "$$lock" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed + source + vendored .crate deps (warm static.crates.io fetches; regenerate the lock on a channel/dep bump)" >&2; exit 1; }; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-uutils.ts" > "$$scratch/uutils.json"; \
	test -s "$$scratch/uutils.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
	sd="$$scratch/b"; mkdir -p "$$sd"; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" TD_BUILDER_PATH="$$TD_BUILDER_PATH" TD_BUILDER_STORE="$$TD_BUILDER_STORE" TD_BUILDER_DB="$$TD_BUILDER_DB" "$$tb" build-recipe "$$scratch/uutils.json" "$$lock" "$$sd" /var/guix/db/db.sqlite > "$$scratch/bout" 2>"$$scratch/err" || { echo "FAIL: build-recipe coreutils build (guix/Guile off PATH):" >&2; tail -30 "$$scratch/err" >&2; exit 1; }; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	if grep -qx 'CACHE=hit' "$$scratch/bout"; then hit=1; else hit=; fi; \
	ns="$$sd/newstore/`basename "$$out"`"; \
	test -x "$$ns/bin/coreutils" || { echo "FAIL: build produced no 'coreutils' multicall binary at $$ns/bin/coreutils" >&2; exit 1; }; \
	grep -q 'TD_VENDOR_CRATES' "$$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_CRATES — the vendored path was not taken" >&2; exit 1; }; \
	test -n "$$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset — load_stage0 did not place a stage0 builder" >&2; exit 1; }; \
	grep -qF "$$TD_BUILDER_PATH/bin/td-builder" "$$sd"/*.drv || { echo "FAIL: the .drv builder is not the stage0 $$TD_BUILDER_PATH — built by the wrong td-builder?" >&2; exit 1; }; \
	echo "  [DURABLE structural] the .drv builder is the td-bootstrapped stage0 ($$TD_BUILDER_PATH) — not the guix-built td-builder (brick 3b)"; \
	if [ -n "$$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — recipe unchanged, reused td's prior coreutils build (no rebuild): $$out"; else echo "  [STRUCTURAL] td assembled + realized the .drv (TD_VENDOR_CRATES, $$ncrate deps) with guix/Guile off PATH: $$out"; fi; \
	bin="$$ns/bin/coreutils"; w="$$scratch/work"; rm -rf "$$w"; mkdir -p "$$w"; \
	"$$bin" mkdir "$$w/sub" || { echo "FAIL: multicall mkdir" >&2; exit 1; }; \
	test -d "$$w/sub" || { echo "FAIL: coreutils mkdir did not create the dir" >&2; exit 1; }; \
	printf 'hello from td-built coreutils\nline two\n' > "$$w/f.txt"; \
	"$$bin" cp "$$w/f.txt" "$$w/sub/g.txt" || { echo "FAIL: multicall cp" >&2; exit 1; }; \
	got=`"$$bin" cat "$$w/sub/g.txt"`; \
	test "$$got" = "$$(printf 'hello from td-built coreutils\nline two')" || { echo "FAIL: coreutils cat did not round-trip the copied file (got: $$got)" >&2; exit 1; }; \
	"$$bin" ls "$$w/sub" | grep -qx 'g.txt' || { echo "FAIL: coreutils ls did not list the copied file" >&2; exit 1; }; \
	"$$bin" mv "$$w/sub/g.txt" "$$w/sub/h.txt" || { echo "FAIL: multicall mv" >&2; exit 1; }; \
	test -e "$$w/sub/h.txt" -a ! -e "$$w/sub/g.txt" || { echo "FAIL: coreutils mv did not move the file" >&2; exit 1; }; \
	"$$bin" rm "$$w/sub/h.txt" || { echo "FAIL: multicall rm" >&2; exit 1; }; \
	test ! -e "$$w/sub/h.txt" || { echo "FAIL: coreutils rm did not remove the file" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the ONE td-built coreutils multicall binary dispatches mkdir/cp/cat/ls/mv/rm — it works as coreutils"; \
	if [ -n "$$hit" ] && [ -f "$$sd/verified-reproducible" ]; then \
	  echo "  [DURABLE repro] CACHED: recipe unchanged + previously verified reproducible — td-builder check skipped (verdict memoized)"; \
	else \
	  rm -rf "$$scratch/chk"; "$$tb" check "$$sd"/*.drv "$$sd/closure.txt" "$$scratch/chk" > "$$scratch/checkout.txt" 2>"$$scratch/chk.err" \
	    || { echo "FAIL: rust-coreutils NOT reproducible (td-builder check):" >&2; tail -6 "$$scratch/checkout.txt" "$$scratch/chk.err" >&2; exit 1; }; \
	  grep -qE "^CHECK out $$out sha256:[0-9a-f]+ reproducible$$" "$$scratch/checkout.txt" \
	    || { echo "FAIL: td-builder check did not confirm $$out reproducible:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	  : > "$$sd/verified-reproducible"; \
	  echo "  [DURABLE repro] td-builder check double-build agrees the 507-crate coreutils build is reproducible"; \
	fi; \
	rm -rf "$$scratch/chk" "$$scratch/tmp" "$$scratch/bout" "$$scratch/err" "$$scratch/checkout.txt" "$$scratch/chk.err" "$$w"; mkdir -p "$$scratch/tmp"; \
	echo "PASS: td built the FULL uutils coreutils multicall (coreutils 0.9.0) from source via td-builder build-recipe — the 507-crate dependency closure + the crate source resolved from pinned static.crates.io fetches (no specification->package, no network), the cargo vendor dir assembled by td's run_rust, the .drv assembled + realized by td (no guix (derivation …) / no guix-daemon), with guix/Guile SCRUBBED FROM PATH; the ONE binary dispatches mkdir/cp/cat/ls/mv/rm (durable) and is reproducible by td's own double-build across the whole graph (durable). The Rust userland, built from source by td. The rustc/cargo/gcc seed + locked deps stay external (§5, retired last)."
