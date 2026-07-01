#!/bin/sh
# crate-free-build.sh — the shared GUIX-FREE rust build for the corpus `rust-<pkg>-crate-free`
# gates. The package's source tree + its FULL dep closure were warmed by
# td-feed warm crate (cargo resolved + fetched them THROUGH td's cargo-proxy, each
# `.crate` sha256 == the crates.io index cksum). This script then, with NO guix in the crate
# path and NO oracle (content-address = the Cargo.lock pin is the oracle):
#   - [supply-chain] asserts every vendored crate's sha256 ∈ the package's shipped Cargo.lock
#   - interns the source tree + the crate set with td's OWN store-add-recursive (no daemon)
#   - builds via `build-recipe` with TD_VENDOR_DIR (no `guix build`, no /gnu/store crate FOD)
#   - [structural] asserts the .drv sets TD_VENDOR_DIR and references NO /gnu/store crate path
#   - [repro] asserts td-builder check's double-build agrees the build is reproducible
# It prints `OUT=<store path>` and `NS=<newstore dir>` on stdout for the caller's package
# specific behavioral assertion; all progress/DURABLE lines go to stderr. The rust/gcc
# toolchain seed stays guix-built (retired last by source-bootstrap).
#
# Usage: crate-free-build.sh NAME CRATEDIR LOCK SOURCEKEY RECIPE
#   NAME       the cache subdir + recipe name (ripgrep, sd, fd, ...).
#   CRATEDIR   the extracted source basename under crate-vendor/NAME/src (e.g. fd-find-10.2.0).
#   LOCK       tests/<x>.lock (its /gnu/store lines are the toolchain seed; .crate lines unused).
#   SOURCEKEY  the lock's source key (ripgrep-source, sd-source, fd-source).
#   RECIPE     tests/ts/recipe-<x>.ts.
# Reads env: TB TD_RECIPE_EVAL TD_BUILDER_PATH TD_BUILDER_STORE TD_BUILDER_DB
#            GUIX(=guix) ROOT(=pwd).
set -eu

name=$1; cratedir=$2; lock=$3; sourcekey=$4; recipe=$5
: "${TB:?TB unset (load_stage0)}" "${TD_BUILDER_PATH:?}"
root=${ROOT:-$(pwd)}
guix=${GUIX:-guix}
dest="$root/.td-build-cache/crate-vendor/$name"
srctree="$dest/src/$cratedir"
vendor="$dest/vendor"
cargolock="$srctree/Cargo.lock"

test -f "$srctree/Cargo.toml" || { echo "ERROR: no source tree at $srctree — the HOST PREP td-feed warm crate (check.sh prelude) must provision it first (offline gate cannot egress)" >&2; exit 1; }
test -f "$cargolock" || { echo "ERROR: source $srctree ships no Cargo.lock" >&2; exit 1; }
ncrate=$(ls "$vendor"/*.crate 2>/dev/null | wc -l)
test "$ncrate" -ge 30 || { echo "ERROR: vendor dir $vendor has <30 crates ($ncrate) — re-run td-feed warm crate" >&2; exit 1; }

miss=0
for c in "$vendor"/*.crate; do
  sha=$(sha256sum "$c" | cut -d' ' -f1)
  grep -qF "$sha" "$cargolock" || { echo "FAIL: crate $(basename "$c") sha $sha is NOT pinned in $name's Cargo.lock" >&2; miss=$((miss + 1)); }
done
test "$miss" -eq 0 || { echo "FAIL: $miss vendored crate(s) not pinned by $name's Cargo.lock" >&2; exit 1; }
echo "  [DURABLE supply-chain] all $ncrate vendored crates' sha256 are checksums pinned in $name's shipped Cargo.lock (== the upstream crates.io cksum the cargo-proxy verified — the guix-free oracle)" >&2

cu=$(grep -- '-coreutils-' "$lock" | sed 's/^[^ ]* //' | head -1)
test -n "$cu" || { echo "ERROR: no coreutils in $lock for the scrubbed PATH" >&2; exit 1; }
if ls "$cu/bin" | grep -qE '^(guix|guile)$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi

scratch="$root/.td-build-cache/$name-crate-free"; rm -rf "$scratch"; mkdir -p "$scratch/tmp" "$scratch/sd"
# Realize ONLY the toolchain seed (rust/gcc/coreutils/bash/tar/gzip) — NOT the crates (guix-free).
grep -v '\.crate ' "$lock" | grep -v "^$sourcekey " | grep ' /gnu/store/' | sed 's/^[^ ]* //' | xargs $guix build >/dev/null || { echo "ERROR: could not realize the toolchain seed" >&2; exit 1; }

srcinfo=$(sh tests/intern-src.sh "$TB" "$name-src" "$srctree" "$scratch/src" target vendor .cargo) || { echo "ERROR: intern source failed" >&2; exit 1; }
eval "$srcinfo"
vinfo=$(sh tests/intern-src.sh "$TB" "$name-vendor" "$vendor" "$scratch/vendor") || { echo "ERROR: intern vendor tree failed" >&2; exit 1; }
vsrc=$(echo "$vinfo" | sed -n "s/^src='\(.*\)'/\1/p")
vstore=$(echo "$vinfo" | sed -n "s/^srcstore='\(.*\)'/\1/p")
vdb=$(echo "$vinfo" | sed -n "s/^srcdb='\(.*\)'/\1/p")
test -n "$vsrc" -a -n "$vstore" -a -n "$vdb" || { echo "ERROR: vendor intern produced no path" >&2; exit 1; }
echo "  [DURABLE structural] td interned the source + the $ncrate-crate set as content-addressed trees (store-add-recursive, no daemon): vendor $vsrc" >&2

seedlock="$scratch/seed.lock"; { grep -v '\.crate ' "$lock" | grep -v "^$sourcekey "; echo "$sourcekey $src"; } > "$seedlock"
_st=$(basename "$recipe" .ts); _st=${_st#recipe-}; sh tests/recipe-emit.sh "$_st" > "$scratch/recipe.json"
test -s "$scratch/recipe.json" || { echo "ERROR: recipe-emit produced no JSON" >&2; exit 1; }
sd="$scratch/sd"
env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$cu/bin" TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" "$TB" build-recipe "$scratch/recipe.json" "$seedlock" "$sd" /gnu/store "$srcstore" "$srcdb" "$vsrc" "$vstore" "$vdb" > "$scratch/bout" 2>"$scratch/err" || { echo "FAIL: build-recipe (guix-free crates):" >&2; tail -40 "$scratch/err" >&2; exit 1; }
out=$(sed -n 's/^OUT=out //p' "$scratch/bout")
test -n "$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$scratch/err" >&2; exit 1; }
ns="$sd/newstore/$(basename "$out")"
grep -q 'TD_VENDOR_DIR' "$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_DIR" >&2; exit 1; }
if grep -oqE '/gnu/store/[a-z0-9]+-[^ /]+\.crate' "$sd"/*.drv; then echo "FAIL: the .drv references a /gnu/store crate path (not guix-free)" >&2; exit 1; fi
echo "  [DURABLE structural] the .drv sets TD_VENDOR_DIR and references NO /gnu/store crate path — crates are guix-free: $out" >&2

rm -rf "$scratch/chk"; "$TB" check "$sd"/*.drv "$sd/closure.txt" "$scratch/chk" > "$scratch/checkout.txt" 2>"$scratch/chk.err" \
  || { echo "FAIL: NOT reproducible (td-builder check):" >&2; tail -6 "$scratch/checkout.txt" "$scratch/chk.err" >&2; exit 1; }
grep -qE "^CHECK out $out sha256:[0-9a-f]+ reproducible$" "$scratch/checkout.txt" \
  || { echo "FAIL: td-builder check did not confirm $out reproducible:" >&2; cat "$scratch/checkout.txt" >&2; exit 1; }
echo "  [DURABLE repro] td-builder check double-build agrees the guix-free-crate $name build is reproducible" >&2

echo "OUT=$out"
echo "NS=$ns"
