# rust-ts-eval — td builds td-ts-eval (its OWN boa-based JS evaluator, ts-eval/) from
# source via `td-builder build-recipe` (buildSystem "rust") by the td-bootstrapped
# stage0 (move-off-Guile §5; bootstrap-ts-eval Brick 4, follow-on to td-builder's
# self-host). td-ts-eval is the SEED TOOL that evaluates TS recipes (ts-emit); today it
# is `guix build -e '(@ (system td-ts) td-ts-eval)'`-produced. This gate builds it with
# td's OWN tooling instead: the boa dependency closure (128 crates) is vendored as
# fixed-output static.crates.io fetches (tests/td-ts-eval.lock, Cargo.lock-pinned,
# TD_VENDOR_CRATES); the ts-eval/ SOURCE is interned by td's own recursive addToStore
# (tests/intern-src.sh); the .drv is assembled + realized by td (no guix (derivation …)
# / no guix-daemon), its builder the stage0 path, with guix/Guile SCRUBBED FROM PATH.
#
# Bootstrap circularity (honest): td-ts-eval's recipe is evaluated by ts-emit, which
# needs A td-ts-eval — the guix-built SEED. So `guix build (system td-ts) td-ts-eval`
# stays here as the seed (evaluates recipe-td-ts-eval.ts) + the behavioral ORACLE, like
# guix-tb in rust-build. Removing it from the OTHER gates' ts-emit (using THIS td-built
# td-ts-eval) is Brick 4b. The rustc/cargo/gcc seed is external, retired LAST (§5).
#
# Per the differential+durable discipline:
#   [DURABLE structural] the .drv's builder is the stage0 path (not the guix-built
#     td-builder), and the .drv carries TD_VENDOR_CRATES (the vendored path was taken).
#   [DURABLE behavioral] the td-built td-ts-eval EVALUATES a probe TS spec to JSON — boa
#     runs, it works as the evaluator.
#   [DURABLE repro] td-builder check's double-build agrees the build is reproducible.
#   [MIGRATION ORACLE, removable] the td-built td-ts-eval evaluates the probe IDENTICALLY
#     to the guix-built td-ts-eval, at a DISTINCT store path — own, then diverge.
HEAVY_GATES += rust-ts-eval
# Ordered AFTER the parallel build-recipes phase (its boa cargo build would otherwise
# oversubscribe cores). Not in BUILD_SPECS — the source is interned at gate time.
BUILD_GATES += rust-ts-eval
rust-ts-eval:
	@echo ">> rust-ts-eval: td builds td-ts-eval (boa evaluator, 128 vendored crates) from source via build-recipe + stage0 (guix/Guile off PATH); it evaluates a TS spec + is reproducible"
	@set -euo pipefail; \
	tgz=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-tsgo-tarball)'`; tsgo=`sh tests/tsgo.sh "$$tgz"`; \
	ev=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-ts-eval)'`/bin/td-ts-eval; \
	test -x "$$ev" -a -n "$$tsgo" -a -x "$$tsgo/lib/tsc" || { echo "ERROR: could not resolve td-tsgo / td-ts-eval (the seed+oracle)" >&2; exit 1; }; \
	export TD_TSGO="$$tsgo" TD_TS_EVAL="$$ev" TD_TSDIR="$(CURDIR)/tests/ts"; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
	lock0="$(CURDIR)/tests/td-ts-eval.lock"; \
	test -s "$$lock0" || { echo "ERROR: no lock $$lock0" >&2; exit 1; }; \
	cu=`grep -- '-coreutils-' "$$lock0" | sed 's/^[^ ]* //' | head -1`; \
	test -n "$$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
	if ls "$$cu/bin" | grep -qE '^(guix|guile)$$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
	ncrate=`grep -cE '\.crate /gnu/store/' "$$lock0"`; \
	test "$$ncrate" -ge 100 || { echo "ERROR: lock has <100 vendored .crate deps ($$ncrate) — regenerate from ts-eval/Cargo.lock" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-build-cache/rust-ts-eval"; mkdir -p "$$scratch/tmp" "$$scratch/b"; rm -f "$$scratch/b/"*.drv; \
	grep ' /gnu/store/' "$$lock0" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null || { echo "ERROR: could not realize the seed + 128 vendored .crate deps (warm static.crates.io fetches; regenerate the lock on a channel/boa bump)" >&2; exit 1; }; \
	srcinfo=`sh tests/intern-src.sh "$$tb" td-ts-eval-src "$(CURDIR)/ts-eval" "$$scratch" target vendor .cargo` || { echo "ERROR: td could not intern the ts-eval crate tree (store-add-recursive)" >&2; exit 1; }; \
	eval "$$srcinfo"; \
	test -n "$$src" -a -d "$$srcstore/`basename "$$src"`" || { echo "ERROR: td interned no ts-eval source tree (store-add-recursive)" >&2; exit 1; }; \
	lock="$$scratch/td-ts-eval.lock"; { cat "$$lock0"; echo "td-ts-eval-source $$src"; } > "$$lock"; \
	sh tests/ts-emit.sh "$(CURDIR)/tests/ts/recipe-td-ts-eval.ts" > "$$scratch/td-ts-eval.json"; \
	test -s "$$scratch/td-ts-eval.json" || { echo "ERROR: ts-emit produced no JSON (the seed td-ts-eval could not evaluate the recipe)" >&2; exit 1; }; \
	sd="$$scratch/b"; mkdir -p "$$sd"; \
	env -i HOME="$$scratch" TMPDIR="$$scratch/tmp" PATH="$$cu/bin" TD_BUILDER_PATH="$$TD_BUILDER_PATH" TD_BUILDER_STORE="$$TD_BUILDER_STORE" TD_BUILDER_DB="$$TD_BUILDER_DB" "$$tb" build-recipe "$$scratch/td-ts-eval.json" "$$lock" "$$sd" /var/guix/db/db.sqlite "$$srcstore" "$$srcdb" > "$$scratch/bout" 2>"$$scratch/err" || { echo "FAIL: build-recipe td-ts-eval build (guix/Guile off PATH):" >&2; tail -30 "$$scratch/err" >&2; exit 1; }; \
	out=`sed -n 's/^OUT=out //p' "$$scratch/bout"`; \
	test -n "$$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$$scratch/err" >&2; exit 1; }; \
	if grep -qx 'CACHE=hit' "$$scratch/bout"; then hit=1; else hit=; fi; \
	ns="$$sd/newstore/`basename "$$out"`"; \
	test -x "$$ns/bin/td-ts-eval" || { echo "FAIL: td-ts-eval build produced no binary at $$ns/bin/td-ts-eval" >&2; exit 1; }; \
	grep -q 'TD_VENDOR_CRATES' "$$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_CRATES — the vendored path was not taken" >&2; exit 1; }; \
	test -n "$$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset — load_stage0 did not place a stage0 builder" >&2; exit 1; }; \
	grep -qF "$$TD_BUILDER_PATH/bin/td-builder" "$$sd"/*.drv || { echo "FAIL: the .drv builder is not the stage0 $$TD_BUILDER_PATH — built by the wrong td-builder?" >&2; exit 1; }; \
	if [ -n "$$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — recipe unchanged, reused td's prior td-ts-eval build (no rebuild): $$out"; else echo "  [STRUCTURAL] td assembled + realized the .drv (TD_VENDOR_CRATES, $$ncrate deps) with guix/Guile off PATH: $$out"; fi; \
	echo "  [DURABLE structural] the .drv builder is the td-bootstrapped stage0 ($$TD_BUILDER_PATH) — not the guix-built td-builder (cargo→stage0→td-ts-eval)"; \
	probe="$(CURDIR)/tests/ts/recipe-hello.ts"; \
	got=`TD_TS_EVAL="$$ns/bin/td-ts-eval" sh tests/ts-emit.sh "$$probe" 2>/dev/null`; \
	want=`TD_TS_EVAL="$$ev" sh tests/ts-emit.sh "$$probe" 2>/dev/null`; \
	test -n "$$got" || { echo "FAIL: the td-built td-ts-eval produced no JSON — it does not evaluate" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] the td-built td-ts-eval EVALUATES a TS spec → JSON (boa runs): $$got"; \
	test "$$got" = "$$want" || { echo "FAIL: td-built and guix-built td-ts-eval DISAGREE on the probe ($$got != $$want)" >&2; exit 1; }; \
	echo "  [MIGRATION ORACLE] it evaluates the probe IDENTICALLY to the guix-built td-ts-eval (behavioral equivalence)"; \
	if [ -n "$$hit" ] && [ -f "$$sd/verified-reproducible" ]; then \
	  echo "  [DURABLE repro] CACHED: recipe unchanged + previously verified reproducible — td-builder check skipped (verdict memoized)"; \
	else \
	  rm -rf "$$scratch/chk"; "$$tb" check "$$sd"/*.drv "$$sd/closure.txt" "$$scratch/chk" > "$$scratch/checkout.txt" 2>"$$scratch/chk.err" \
	    || { echo "FAIL: rust-ts-eval NOT reproducible (td-builder check):" >&2; tail -6 "$$scratch/checkout.txt" "$$scratch/chk.err" >&2; exit 1; }; \
	  grep -qE "^CHECK out $$out sha256:[0-9a-f]+ reproducible$$" "$$scratch/checkout.txt" \
	    || { echo "FAIL: td-builder check did not confirm $$out reproducible:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	  : > "$$sd/verified-reproducible"; \
	  echo "  [DURABLE repro] td-builder check double-build agrees the td-ts-eval build is reproducible"; \
	fi; \
	gtse=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-ts-eval)'`; \
	if [ "$$out" = "$$gtse" ]; then echo "FAIL: td's td-ts-eval path equals guix's — expected a distinct own-builder path" >&2; exit 1; fi; \
	echo "  [MIGRATION ORACLE, removable] distinct from guix's td-ts-eval ($$gtse)"; \
	rm -rf "$$scratch/chk" "$$scratch/tmp" "$$scratch/bout" "$$scratch/err" "$$scratch/checkout.txt" "$$scratch/chk.err"; mkdir -p "$$scratch/tmp"; \
	echo "PASS: td built td-ts-eval (its boa-based JS evaluator, 128 vendored crate deps) from source via td-builder build-recipe — the dependency closure resolved from pinned static.crates.io fetches (no specification->package, no network), the cargo vendor dir assembled by td's run_rust, the ts-eval/ source interned by td's own recursive addToStore, the .drv assembled + realized by td (no guix (derivation …) / no guix-daemon) with its BUILDER the td-bootstrapped stage0 and guix/Guile SCRUBBED FROM PATH; the td-built td-ts-eval EVALUATES a TS spec (boa runs, durable), agrees with the guix-built td-ts-eval (migration oracle), is reproducible by td's own double-build (durable), and lands at a distinct store path (own, then diverge). The guix-built td-ts-eval stays only as the SEED that evaluates the recipe + the oracle (Brick 4b swaps the other gates' ts-emit onto this td-built one); the rustc/cargo/gcc seed is external (§5, retired last)."
