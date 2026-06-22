# rust-sd — td builds `sd` (the intuitive find-and-replace, a `sed` alternative) FROM SOURCE,
# continuing the shipped Rust userland (procs/fd/ripgrep/sd/eza/bat, PR #80) from guix-packaged
# to td-built (the uutils-`cat` pattern; move-off-Guile §5). `sd` 1.0.0 is built via
# `td-builder build-recipe` (buildSystem "rust") with its FULL 111-crate closure vendored as
# fixed-output static.crates.io fetches (sha256 == each Cargo.lock checksum), pinned in
# tests/sd.lock. Pure Rust, no C build. The .drv is td-assembled (no guix (derivation …)) +
# realized daemon-free (no guix-daemon), guix/Guile SCRUBBED FROM PATH. The rustc/cargo/gcc
# seed is external (§5). Auto-classified self-hosted by buildSystem (no census enrollment).
#
# ALL-DURABLE (no guix oracle): each leg stands with no Guix in the room:
#   [STRUCTURAL] the build runs guix/Guile off PATH, the .drv carries TD_VENDOR_CRATES and its
#     builder is the td-bootstrapped stage0.
#   [DURABLE behavioral] the built `sd` does a real find-and-replace (and leaves a non-match
#     unchanged) — real sd behavior, not just --version.
#   [DURABLE repro] td-builder check's double-build agrees the build is reproducible across the
#     whole 111-crate graph.
HEAVY_GATES += rust-sd
# Self-contained lock (tests/sd.lock) — built by the gate itself; ordered after build-recipes
# (a BUILD_GATE) so its cargo build doesn't oversubscribe the corpus fan-out.
BUILD_GATES += rust-sd
rust-sd:
	@echo ">> rust-sd: td builds 'sd' (1.0.0, 111 vendored deps) from source via build-recipe (offline, guix/Guile off PATH); it find-and-replaces + is reproducible"
	@set -euo pipefail; \
	tsgo=`sh tests/tsgo.sh`; \
	test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: could not resolve td-tsgo" >&2; exit 1; }; \
	export TD_TSGO="$$tsgo" TD_TSDIR="$(CURDIR)/tests/ts"; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_ts_eval; tb="$$TB"; \
	case "$$TD_TS_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_TS_EVAL is not td's own build ($$TD_TS_EVAL)" >&2; exit 1 ;; esac; \
	echo "  [DURABLE structural] ts-emit evaluates with td's OWN td-ts-eval ($$TD_TS_EVAL) — not the guix-built one (brick 4c)"; \
	lock="$(CURDIR)/tests/sd.lock"; \
	test -s "$$lock" || { echo "ERROR: no lock $$lock" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-' "$$lock" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	ncrate=`grep -cE '\.crate /gnu/store/' "$$lock"`; \
	test "$$ncrate" -ge 100 || { echo "ERROR: lock has <100 vendored .crate deps ($$ncrate) — regenerate from sd's Cargo.lock" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-build-cache/rust-sd"; mkdir -p "$$scratch/tmp" "$$scratch/b"; rm -f "$$scratch/b/"*.drv; \
	grep ' /gnu/store/' "$$lock" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed + source + vendored .crate deps (warm static.crates.io fetches; regenerate the lock on a channel/dep bump)" >&2; exit 1; }; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-sd.ts" > "$$scratch/sd.json"; \
	test -s "$$scratch/sd.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
	sd="$$scratch/b"; mkdir -p "$$sd"; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" TD_BUILDER_PATH="$$TD_BUILDER_PATH" TD_BUILDER_STORE="$$TD_BUILDER_STORE" TD_BUILDER_DB="$$TD_BUILDER_DB" "$$tb" build-recipe "$$scratch/sd.json" "$$lock" "$$sd" /var/guix/db/db.sqlite > "$$scratch/bout" 2>"$$scratch/err" || { echo "FAIL: build-recipe sd build (guix/Guile off PATH):" >&2; tail -30 "$$scratch/err" >&2; exit 1; }; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	if grep -qx 'CACHE=hit' "$$scratch/bout"; then hit=1; else hit=; fi; \
	ns="$$sd/newstore/`basename "$$out"`"; \
	test -x "$$ns/bin/sd" || { echo "FAIL: sd build produced no 'sd' binary at $$ns/bin/sd" >&2; exit 1; }; \
	grep -q 'TD_VENDOR_CRATES' "$$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_CRATES — the vendored path was not taken" >&2; exit 1; }; \
	test -n "$$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset — load_stage0 did not place a stage0 builder" >&2; exit 1; }; \
	grep -qF "$$TD_BUILDER_PATH/bin/td-builder" "$$sd"/*.drv || { echo "FAIL: the .drv builder is not the stage0 $$TD_BUILDER_PATH — built by the wrong td-builder?" >&2; exit 1; }; \
	echo "  [DURABLE structural] the .drv builder is the td-bootstrapped stage0 ($$TD_BUILDER_PATH) — not the guix-built td-builder (brick 3b)"; \
	if [ -n "$$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — recipe unchanged, reused td's prior sd build (no rebuild): $$out"; else echo "  [STRUCTURAL] td assembled + realized the .drv (TD_VENDOR_CRATES, $$ncrate deps) with guix/Guile off PATH: $$out"; fi; \
	got=`printf 'hello world\n' | "$$ns/bin/sd" 'world' 'there'`; \
	test "$$got" = "hello there" || { echo "FAIL: td-built sd did not replace world->there (got: $$got)" >&2; exit 1; }; \
	unchanged=`printf 'hello world\n' | "$$ns/bin/sd" 'zzznomatch' 'X'`; \
	test "$$unchanged" = "hello world" || { echo "FAIL: sd altered input on a non-matching pattern (got: $$unchanged)" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built 'sd' replaced world->there (and left a non-match unchanged) — it works as sd"; \
	if [ -n "$$hit" ] && [ -f "$$sd/verified-reproducible" ]; then \
	  echo "  [DURABLE repro] CACHED: recipe unchanged + previously verified reproducible — td-builder check skipped (verdict memoized)"; \
	else \
	  rm -rf "$$scratch/chk"; "$$tb" check "$$sd"/*.drv "$$sd/closure.txt" "$$scratch/chk" > "$$scratch/checkout.txt" 2>"$$scratch/chk.err" \
	    || { echo "FAIL: rust-sd NOT reproducible (td-builder check):" >&2; tail -6 "$$scratch/checkout.txt" "$$scratch/chk.err" >&2; exit 1; }; \
	  grep -qE "^CHECK out $$out sha256:[0-9a-f]+ reproducible$$" "$$scratch/checkout.txt" \
	    || { echo "FAIL: td-builder check did not confirm $$out reproducible:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	  : > "$$sd/verified-reproducible"; \
	  echo "  [DURABLE repro] td-builder check double-build agrees the 111-crate sd build is reproducible"; \
	fi; \
	rm -rf "$$scratch/chk" "$$scratch/tmp" "$$scratch/bout" "$$scratch/err" "$$scratch/checkout.txt" "$$scratch/chk.err"; mkdir -p "$$scratch/tmp"; \
	echo "PASS: td built 'sd' (1.0.0) from source via td-builder build-recipe — the full 111-crate dependency closure + the crate source resolved from pinned static.crates.io fetches (no specification->package, no network), the cargo vendor dir assembled by td's run_rust, the .drv assembled + realized by td (no guix (derivation …) / no guix-daemon), with guix/Guile SCRUBBED FROM PATH; the binary works as sd (find-and-replace, durable) and is reproducible by td's own double-build across the whole graph (durable). Another shipped Rust userland tool, built from source by td. The rustc/cargo/gcc seed + locked deps stay external (§5, retired last)."
