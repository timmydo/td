# rust-uutils — td builds a REAL coreutils replacement from source (rust-build
# Inc.3; move-off-Guile §5). The uutils `cat` (crate uu_cat 0.9.0, binary `cat`) is
# built via `td-builder build-recipe` (buildSystem "rust") with its FULL dependency
# closure — 139 crates — vendored: each is a fixed-output fetch from
# static.crates.io (sha256 == its Cargo.lock checksum), pinned in
# tests/cat-uutils.lock; the source is the upstream uu_cat crate tarball, also a
# lock-supplied fetch. run_rust assembles the cargo vendor dir from them and builds
# `cargo --offline --frozen`; the .drv is assembled by td (no guix (derivation …))
# and realized daemon-free (no guix-daemon), with guix/Guile SCRUBBED FROM PATH.
# No builder code change — Inc.1 (tarball source) + Inc.2 (vendored deps) already
# cover it. The rustc/cargo/gcc seed + locked deps are the external SEED (§5).
#
# ALL-DURABLE (no guix oracle): there is no guix build of uu_cat to diff against —
# every leg stands with no Guix in the room:
#   [STRUCTURAL] the build runs guix/Guile off PATH, produces the binary, and the
#     .drv carries TD_VENDOR_CRATES.
#   [DURABLE behavioral] the built `cat` round-trips a file AND a stdin pipe — it
#     actually works as cat (real coreutils behavior, not just --version).
#   [DURABLE repro] td-builder check's double-build agrees the build is reproducible
#     across the whole 139-crate graph (proc-macros, build scripts, fluent locales).
HEAVY_GATES += rust-uutils
# Ordered AFTER the parallel build-recipes phase (its 139-crate cargo build would
# otherwise oversubscribe cores against build-recipes' fan-out). Not in BUILD_SPECS —
# its lock (tests/cat-uutils.lock) is self-contained, so the gate builds it itself.
BUILD_GATES += rust-uutils
rust-uutils:
	@echo ">> rust-uutils: td builds the uutils 'cat' (uu_cat 0.9.0, 139 vendored deps) from source via build-recipe (offline, guix/Guile off PATH); it works as cat + is reproducible"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	ev=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-ts-eval)'`/bin/td-ts-eval; \
	test -x "$$ev" -a -x "$$node" -a -n "$$tsc" || { echo "ERROR: could not resolve node / tsc / ts-eval" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
	lock="$(CURDIR)/tests/cat-uutils.lock"; \
	test -s "$$lock" || { echo "ERROR: no lock $$lock" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-' "$$lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	ncrate=`grep -cE '\.crate /gnu/store/' "$$lock"`; \
	test "$$ncrate" -ge 100 || { echo "ERROR: lock has <100 vendored .crate deps ($$ncrate) — regenerate from uu_cat's Cargo.lock" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-build-cache/rust-uutils"; mkdir -p "$$scratch/tmp" "$$scratch/b"; rm -f "$$scratch/b/"*.drv; \
	grep ' /gnu/store/' "$$lock" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed + source + vendored .crate deps (warm static.crates.io fetches; regenerate the lock on a channel/dep bump)" >&2; exit 1; }; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-cat.ts" > "$$scratch/cat.json"; \
	test -s "$$scratch/cat.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
	sd="$$scratch/b"; mkdir -p "$$sd"; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" TD_BUILDER_PATH="$$TD_BUILDER_PATH" TD_BUILDER_STORE="$$TD_BUILDER_STORE" TD_BUILDER_DB="$$TD_BUILDER_DB" "$$tb" build-recipe "$$scratch/cat.json" "$$lock" "$$sd" /var/guix/db/db.sqlite > "$$scratch/bout" 2>"$$scratch/err" || { echo "FAIL: build-recipe uu_cat build (guix/Guile off PATH):" >&2; tail -30 "$$scratch/err" >&2; exit 1; }; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	if grep -qx 'CACHE=hit' "$$scratch/bout"; then hit=1; else hit=; fi; \
	ns="$$sd/newstore/`basename "$$out"`"; \
	test -x "$$ns/bin/cat" || { echo "FAIL: uu_cat build produced no 'cat' binary at $$ns/bin/cat" >&2; exit 1; }; \
	grep -q 'TD_VENDOR_CRATES' "$$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_CRATES — the vendored path was not taken" >&2; exit 1; }; \
	test -n "$$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset — load_stage0 did not place a stage0 builder" >&2; exit 1; }; \
	grep -qF "$$TD_BUILDER_PATH/bin/td-builder" "$$sd"/*.drv || { echo "FAIL: the .drv builder is not the stage0 $$TD_BUILDER_PATH — built by the wrong td-builder?" >&2; exit 1; }; \
	echo "  [DURABLE structural] the .drv builder is the td-bootstrapped stage0 ($$TD_BUILDER_PATH) — not the guix-built td-builder (brick 3b)"; \
	if [ -n "$$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — recipe unchanged, reused td's prior uu_cat build (no rebuild): $$out"; else echo "  [STRUCTURAL] td assembled + realized the .drv (TD_VENDOR_CRATES, $$ncrate deps) with guix/Guile off PATH: $$out"; fi; \
	printf 'hello from td-built cat\nline two\n' > "$$scratch/in.txt"; \
	got=`"$$ns/bin/cat" "$$scratch/in.txt"`; \
	test "$$got" = "$$(printf 'hello from td-built cat\nline two')" || { echo "FAIL: td-built cat did not round-trip the file (got: $$got)" >&2; exit 1; }; \
	piped=`printf 'piped-in\n' | "$$ns/bin/cat"`; \
	test "$$piped" = "piped-in" || { echo "FAIL: td-built cat did not round-trip stdin (got: $$piped)" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built uutils 'cat' round-trips a file AND a stdin pipe — it works as cat"; \
	if [ -n "$$hit" ] && [ -f "$$sd/verified-reproducible" ]; then \
	  echo "  [DURABLE repro] CACHED: recipe unchanged + previously verified reproducible — td-builder check skipped (verdict memoized)"; \
	else \
	  rm -rf "$$scratch/chk"; "$$tb" check "$$sd"/*.drv "$$sd/closure.txt" "$$scratch/chk" > "$$scratch/checkout.txt" 2>"$$scratch/chk.err" \
	    || { echo "FAIL: rust-uutils NOT reproducible (td-builder check):" >&2; tail -6 "$$scratch/checkout.txt" "$$scratch/chk.err" >&2; exit 1; }; \
	  grep -qE "^CHECK out $$out sha256:[0-9a-f]+ reproducible$$" "$$scratch/checkout.txt" \
	    || { echo "FAIL: td-builder check did not confirm $$out reproducible:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	  : > "$$sd/verified-reproducible"; \
	  echo "  [DURABLE repro] td-builder check double-build agrees the 139-crate uu_cat build is reproducible"; \
	fi; \
	rm -rf "$$scratch/chk" "$$scratch/tmp" "$$scratch/bout" "$$scratch/err" "$$scratch/checkout.txt" "$$scratch/chk.err" "$$scratch/in.txt"; mkdir -p "$$scratch/tmp"; \
	echo "PASS: td built the uutils 'cat' (uu_cat 0.9.0) from source via td-builder build-recipe — the full 139-crate dependency closure + the crate source resolved from pinned static.crates.io fetches (no specification->package, no network), the cargo vendor dir assembled by td's run_rust, the .drv assembled + realized by td (no guix (derivation …) / no guix-daemon), with guix/Guile SCRUBBED FROM PATH; the binary works as cat (file + stdin round-trip, durable) and is reproducible by td's own double-build across the whole graph (durable). A real Rust coreutils replacement, built from source by td. The rustc/cargo/gcc seed + locked deps stay external (§5, retired last)."
