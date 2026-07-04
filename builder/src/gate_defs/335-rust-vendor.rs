//! rust-vendor — td builds a Rust crate WITH dependencies via its own builder
//! (rust-build Inc.2; move-off-Guile §5). The self-host (gate rust-build) builds a
//! zero-dep crate; this proves the DEPENDENCY path: the td-vendor-demo crate depends
//! on itoa + ryu, whose `.crate`s are fixed-output fetches from static.crates.io
//! (sha256 == their Cargo.lock checksum), pinned in tests/td-vendor-demo.lock.
//! `td-builder build-recipe` routes the `*.crate` entries to TD_VENDOR_CRATES;
//! run_rust assembles a cargo `vendored-sources` dir from them and builds `cargo
//! --offline --frozen` against it (no network). The whole BUILD runs with guix/Guile
//! SCRUBBED FROM PATH; the .drv is assembled by td (no guix (derivation …)) and
//! realized daemon-free (no guix-daemon). The rustc/cargo/gcc seed + the locked deps
//! are the external SEED (§5, retired last).
//! 
//! ALL-DURABLE (no guix oracle): there is no guix cargo-build-system build of this
//! crate to diff against — by design (it is a new capability, not a corpus
//! reconstruction), so every leg stands with no Guix in the room:
//! [STRUCTURAL] the build runs guix/Guile off PATH and produces the binary, AND the
//! .drv carries TD_VENDOR_CRATES (the vendored path was taken, not a dep-free build).
//! [DURABLE behavioral] the binary runs and prints "2026 3.14159" — itoa formats the
//! int, ryu the float, so BOTH vendored deps are exercised.
//! [DURABLE repro] td-builder check's double-build agrees the output is reproducible.
//! Ordered AFTER the parallel build-recipes phase (its cargo build would otherwise
//! oversubscribe cores against build-recipes' fan-out). Not in BUILD_SPECS — the source
//! is interned at gate time by td's OWN recursive addToStore (tests/intern-src.sh →
//! store-add-recursive, no `guix repl`; move-off-Guile §5), so the gate is self-contained.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "rust-vendor",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> rust-vendor: td builds td-vendor-demo (depends on itoa + ryu) via build-recipe with VENDORED deps (offline, guix/Guile off PATH); it runs + is reproducible"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; load_recipe_eval; tb="$TB"; \
case "$TD_RECIPE_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_RECIPE_EVAL is not td's own build ($TD_RECIPE_EVAL)" >&2; exit 1 ;; esac; \
echo "  [DURABLE structural] recipes evaluate with td's OWN td-recipe-eval ($TD_RECIPE_EVAL) — not the guix-built one (brick 4c)"; \
lock0="$PWD/tests/td-vendor-demo.lock"; \
test -s "$lock0" || { echo "ERROR: no lock $lock0" >&2; exit 1; }; \
cu=`grep -- '-coreutils-' "$lock0" | sed 's/^[^ ]* //' | head -1`; \
test -n "$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
if ls "$cu/bin" | grep -qE '^(guix|guile)$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
ncrate=`grep -cE '\.crate /gnu/store/' "$lock0"`; \
test "$ncrate" -ge 2 || { echo "ERROR: lock has <2 vendored .crate deps ($ncrate)" >&2; exit 1; }; \
scratch="$PWD/.td-build-cache/rust-vendor"; mkdir -p "$scratch/tmp" "$scratch/b"; rm -f "$scratch/b/"*.drv; \
grep ' /gnu/store/' "$lock0" | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null || { echo "ERROR: could not realize the seed + vendored .crate deps (warm static.crates.io fetches; regenerate the lock on a channel/dep bump)" >&2; exit 1; }; \
srcinfo=`sh tests/intern-src.sh "$tb" td-vendor-demo-src "$PWD/tests/vendor-demo" "$scratch" target .cargo` || { echo "ERROR: td could not intern the vendor-demo crate tree (store-add-recursive)" >&2; exit 1; }; \
eval "$srcinfo"; \
test -n "$src" -a -d "$srcstore/`basename "$src"`" || { echo "ERROR: td interned no vendor-demo source tree (store-add-recursive)" >&2; exit 1; }; \
lock="$scratch/td-vendor-demo.lock"; { cat "$lock0"; echo "td-vendor-demo-source $src"; } > "$lock"; \
sh tests/recipe-emit.sh td-vendor-demo > "$scratch/td-vendor-demo.json"; \
test -s "$scratch/td-vendor-demo.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }; \
sd="$scratch/b"; mkdir -p "$sd"; \
env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$cu/bin" TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" "$tb" build-recipe "$scratch/td-vendor-demo.json" "$lock" "$sd" /gnu/store "$srcstore" "$srcdb" > "$scratch/bout" 2>"$scratch/err" || { echo "FAIL: build-recipe vendored build (guix/Guile off PATH):" >&2; tail -30 "$scratch/err" >&2; exit 1; }; \
out=`sed -n 's/^OUT=out //p' "$scratch/bout"`; \
test -n "$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$scratch/err" >&2; exit 1; }; \
if grep -qx 'CACHE=hit' "$scratch/bout"; then hit=1; else hit=; fi; \
ns="$sd/newstore/`basename "$out"`"; \
test -x "$ns/bin/td-vendor-demo" || { echo "FAIL: vendored build produced no binary at $ns/bin/td-vendor-demo" >&2; exit 1; }; \
grep -q 'TD_VENDOR_CRATES' "$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_CRATES — the vendored path was not taken" >&2; exit 1; }; \
test -n "$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset — load_stage0 did not place a stage0 builder" >&2; exit 1; }; \
grep -qF "$TD_BUILDER_PATH/bin/td-builder" "$sd"/*.drv || { echo "FAIL: the .drv builder is not the stage0 $TD_BUILDER_PATH — built by the wrong td-builder?" >&2; exit 1; }; \
echo "  [DURABLE structural] the .drv builder is the td-bootstrapped stage0 ($TD_BUILDER_PATH) — not the guix-built td-builder (brick 3b)"; \
if [ -n "$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — recipe unchanged, reused td's prior vendored build (no rebuild): $out"; else echo "  [STRUCTURAL] td assembled + realized the .drv (TD_VENDOR_CRATES, $ncrate deps) with guix/Guile off PATH: $out"; fi; \
got=`"$ns/bin/td-vendor-demo"`; \
test "$got" = "2026 3.14159" || { echo "FAIL: td-vendor-demo printed '$got', expected '2026 3.14159' (itoa + ryu must both work)" >&2; exit 1; }; \
echo "  [DURABLE behavioral] the vendored binary RUNS and prints '$got' (itoa formats the int, ryu the float — both deps exercised)"; \
if [ -n "$hit" ] && [ -f "$sd/verified-reproducible" ]; then \
  echo "  [DURABLE repro] CACHED: recipe unchanged + previously verified reproducible — td-builder check skipped (verdict memoized)"; \
else \
  rm -rf "$scratch/chk"; "$tb" check-drv "$sd"/*.drv "$sd/closure.txt" "$scratch/chk" > "$scratch/checkout.txt" 2>"$scratch/chk.err" \
    || { echo "FAIL: rust-vendor NOT reproducible (td-builder check):" >&2; tail -6 "$scratch/checkout.txt" "$scratch/chk.err" >&2; exit 1; }; \
  grep -qE "^CHECK out $out sha256:[0-9a-f]+ reproducible$" "$scratch/checkout.txt" \
    || { echo "FAIL: td-builder check did not confirm $out reproducible:" >&2; cat "$scratch/checkout.txt" >&2; exit 1; }; \
  : > "$sd/verified-reproducible"; \
  echo "  [DURABLE repro] td-builder check double-build agrees the vendored build is reproducible"; \
fi; \
rm -rf "$scratch/chk" "$scratch/tmp" "$scratch/bout" "$scratch/err" "$scratch/checkout.txt" "$scratch/chk.err"; mkdir -p "$scratch/tmp"; \
echo "PASS: td built td-vendor-demo (a crate WITH deps: itoa + ryu) via td-builder build-recipe — the dependency closure resolved from pinned vendored .crate fetches (no specification->package, no network), the cargo vendor dir assembled by td's run_rust, the .drv assembled + realized by td (no guix (derivation …) / no guix-daemon), with guix/Guile SCRUBBED FROM PATH; the binary runs + exercises both deps (durable) and is reproducible by td's own double-build (durable). The rustc/cargo/gcc seed + locked deps stay external (§5, retired last)."
"##,
    }
}
