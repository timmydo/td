//! rust-build — td-builder self-hosts through its OWN build path, end to end (the
//! Guix cargo-build-system replacement; move-off-Guile §5). td builds td-builder
//! ITSELF via `td-builder build-recipe` on a `buildSystem: "rust"` recipe
//! (tests/ts/recipe-td-builder.ts, authored in TS, emitted Guile-free by ts-eval):
//! every input is resolved from a pinned lock (tests/td-builder-rust.lock, no
//! specification->package), the `.drv` is ASSEMBLED by td (store::assemble_drv — no
//! guix (derivation …)), and it is REALIZED daemon-free (realize_drv — no
//! guix-daemon). The whole BUILD runs with guix/Guile SCRUBBED FROM PATH. So nothing
//! in td-builder's own build path is guix/Guile — only the rustc/cargo/gcc seed and
//! the lock stay external (§5, retired LAST), exactly as the toolchain-no-guix gate.
//! This routes the self-host onto the same build-recipe rail the corpus/toolchain
//! leaves and nano use (own-builder-daemon, #74), replacing the earlier Guile
//! `(derivation …)` lowering.
//! 
//! The source is the LIVE builder/ tree (it changes every edit), so the gate interns
//! the CURRENT tree with td's OWN recursive addToStore (tests/intern-src.sh →
//! `td-builder store-add-recursive`, the gate-285 primitive) into a td-owned store dir +
//! db — NO `guix repl … lower-object` daemon interning (move-off-Guile §5) — and appends
//! the content-addressed path to the committed seed lock. build-recipe is handed that
//! store dir + db so it stages the source from there + reads its closure from the td db.
//! Per the differential+durable discipline:
//! [STRUCTURAL] the build runs with guix/Guile off PATH and produces td-builder.
//! [DURABLE behavioral] the td-built td-builder RUNS (nar-hash) — a functioning builder,
//! not just a compile that exits 0.
//! [DURABLE repro] td-builder check's double-build agrees the output is
//! reproducible (td's own oracle, not guix build --check).
//! (The guix-vs-td differential oracle legs — behavioral equivalence to, and a distinct
//! store path from, guix's cargo-build-system td-builder — were RETIRED in R2 (#275,
//! guix-as-packager surface → 0). The durable legs above ARE the feature.)
//! Heavy (a bootstrap td-builder compile + a cargo self-host build + a double-build
//! check), so it slots in the heavy pool with the other td gates.
//! Ordered AFTER the parallel build phase (its cargo self-host build would otherwise use
//! all cores concurrently with build-recipes' fan-out). td-builder is NOT in BUILD_SPECS
//! — its lock is extended with the freshly-interned source, so it stays self-contained.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "rust-build",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        // Typed artifact input (#353): the scrubbed-PATH coreutils — resolved by
        // the runner from this gate's lock; the body's lock-grepping is deleted.
        inputs: &[ArtifactInput {
            name: "coreutils",
            kind: InputKind::LockEntry { lock: "tests/td-builder-rust.lock", stem: "coreutils" },
        }],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> rust-build: td self-hosts td-builder via build-recipe (buildSystem rust) — .drv assembled + realized by td, guix/Guile off PATH; it runs and is reproducible"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; load_recipe_eval; \
case "$TD_RECIPE_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_RECIPE_EVAL is not td's own build ($TD_RECIPE_EVAL)" >&2; exit 1 ;; esac; \
echo "  [DURABLE structural] recipes evaluate with td's OWN td-recipe-eval ($TD_RECIPE_EVAL) — not the guix-built one (brick 4c)"; \
lock0="$PWD/tests/td-builder-rust.lock"; \
test -s "$lock0" || { echo "ERROR: no lock $lock0" >&2; exit 1; }; \
cu=${TD_GATE_INPUT_COREUTILS:-}; \
test -n "$cu" || { echo "ERROR: TD_GATE_INPUT_COREUTILS unset — run via td-builder gate-run, which resolves the gate's declared inputs" >&2; exit 1; }; \
if ls "$cu/bin" | grep -qE '^(guix|guile)$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
scratch="$PWD/.td-build-cache/rust-build"; mkdir -p "$scratch/tmp" "$scratch/b"; rm -f "$scratch/b/"*.drv; \
grep ' /gnu/store/' "$lock0" | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null || { echo "ERROR: could not realize the rust seed (regenerate the lock on a channel bump)" >&2; exit 1; }; \
srcinfo=`sh tests/intern-src.sh "$TB" td-builder-src "$PWD/builder" "$scratch" target .cargo` || { echo "ERROR: td could not intern the current builder tree (store-add-recursive)" >&2; exit 1; }; \
eval "$srcinfo"; \
test -n "$src" -a -d "$srcstore/`basename "$src"`" || { echo "ERROR: td interned no source tree (store-add-recursive)" >&2; exit 1; }; \
echo ">> td interned the CURRENT builder tree (recursive addToStore, no guix repl / no daemon): $src"; \
lock="$scratch/td-builder-rust.lock"; { cat "$lock0"; echo "td-builder-source $src"; } > "$lock"; \
sh tests/recipe-emit.sh td-builder > "$scratch/td-builder.json"; \
test -s "$scratch/td-builder.json" || { echo "ERROR: ts-emit produced no JSON for td-builder" >&2; exit 1; }; \
grep -q '"buildSystem":"rust"' "$scratch/td-builder.json" || { echo "FAIL: recipe JSON is not buildSystem rust" >&2; cat "$scratch/td-builder.json" >&2; exit 1; }; \
sd="$scratch/b"; mkdir -p "$sd"; \
env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$cu/bin" TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" "$TB" build-recipe "$scratch/td-builder.json" "$lock" "$sd" /gnu/store "$srcstore" "$srcdb" > "$scratch/bout" 2>"$scratch/err" || { echo "FAIL: build-recipe self-host (guix/Guile off PATH):" >&2; tail -30 "$scratch/err" >&2; exit 1; }; \
out=`sed -n 's/^OUT=out //p' "$scratch/bout"`; \
test -n "$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$scratch/err" >&2; exit 1; }; \
if grep -qx 'CACHE=hit' "$scratch/bout"; then hit=1; else hit=; grep -q 'no guix (derivation), no Guile' "$scratch/err" || { echo "FAIL: build-recipe did not assemble the .drv itself" >&2; cat "$scratch/err" >&2; exit 1; }; fi; \
ns="$sd/newstore/`basename "$out"`"; \
test -x "$ns/bin/td-builder" || { echo "FAIL: self-host produced no td-builder binary at $ns/bin/td-builder" >&2; exit 1; }; \
test -n "$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset — load_stage0 did not place a stage0 builder" >&2; exit 1; }; \
grep -qF "$TD_BUILDER_PATH/bin/td-builder" "$sd"/*.drv || { echo "FAIL: the self-host .drv builder is not the stage0 $TD_BUILDER_PATH — built by the wrong td-builder?" >&2; exit 1; }; \
echo "  [DURABLE structural] the self-host .drv builder is the td-bootstrapped stage0 ($TD_BUILDER_PATH) — cargo→stage0→td-builder, no guix-built td-builder in the build (brick 3b)"; \
if [ -n "$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — builder source unchanged, reused td's prior self-host build (no rebuild): $out"; else echo "  [STRUCTURAL] td assembled + realized the .drv with guix/Guile off PATH: $out"; fi; \
printf 'td rust-build behavioral probe\n' > "$scratch/probe"; \
h_td=`"$ns/bin/td-builder" nar-hash "$scratch/probe"`; \
test -n "$h_td" || { echo "FAIL: the td-built td-builder did not run / produced no nar-hash" >&2; exit 1; }; \
echo "  [DURABLE behavioral] the td-built td-builder RUNS: nar-hash = $h_td"; \
if [ -n "$hit" ] && [ -f "$sd/verified-reproducible" ]; then \
  echo "  [DURABLE repro] CACHED: builder source unchanged + previously verified reproducible — td-builder check skipped (verdict memoized)"; \
else \
  rm -rf "$scratch/chk"; "$TB" check-drv "$sd"/*.drv "$sd/closure.txt" "$scratch/chk" > "$scratch/checkout.txt" 2>"$scratch/chk.err" \
    || { echo "FAIL: rust-build NOT reproducible (td-builder check):" >&2; cat "$scratch/checkout.txt" "$scratch/chk.err" >&2; exit 1; }; \
  grep -qE "^CHECK out $out sha256:[0-9a-f]+ reproducible$" "$scratch/checkout.txt" \
    || { echo "FAIL: td-builder check did not confirm $out reproducible:" >&2; cat "$scratch/checkout.txt" >&2; exit 1; }; \
  : > "$sd/verified-reproducible"; \
  echo "  [DURABLE repro] td-builder check double-build agrees the rust-build output is reproducible"; \
fi; \
rm -rf "$scratch/chk" "$scratch/tmp" "$scratch/bout" "$scratch/err" "$scratch/checkout.txt" "$scratch/chk.err"; mkdir -p "$scratch/tmp"; \
echo "PASS: td self-hosted td-builder via build-recipe (buildSystem rust) — every input resolved from a pinned lock (no specification->package), the .drv assembled by td (no guix (derivation …)) and realized daemon-free (no guix-daemon), with guix/Guile SCRUBBED FROM PATH; the output RUNS (durable behavioral) and is reproducible by td's own double-build (durable). The guix-vs-td differential oracle legs were retired in R2 (#275, guix-as-packager surface → 0). The rustc/cargo/gcc seed stays external (§5, retired last)."
"##,
    }
}
