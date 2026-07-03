#!/bin/sh
# tests/rust-seed.sh — RUST IN THE SEED (North-Star, human 2026-06-21): td builds its own
# Rust BUILD ENGINE (td-builder) from a FROZEN SEED that carries the rust toolchain, with
# NO guix install in the build path. This is the Rust analog of the PR3 seed-build gate
# (which built hello/C from a seed) — the step that proves the seed mechanism extends to
# the toolchain td can't self-build (no rust-from-source; "it takes rust to build rust").
#
# Flow (composes the existing primitives — no builder code change):
#   - load_stage0 places the cargo-built stage0 td-builder (the build DRIVER); load_recipe_eval
#     gives td's own td-recipe-eval; the source is the LIVE builder/ tree interned by td's own
#     store-add-recursive (tests/intern-src.sh) — NOT frozen (it changes every edit).
#   - tools/warm-seed.sh captures + unpacks the RUST TOOLCHAIN closure (tests/td-builder-rust.lock
#     roots ∪ stage0's runtime refs) ONCE into a reusable content-addressed cache (the #135
#     warm-seed rail — no 2GB re-capture per run), no daemon, no /gnu/store write.
#   - `build-recipe` builds td-builder (recipe-td-builder.ts, buildSystem rust) with the
#     unpacked seed as its store DB (TD_SEED_STORE/TD_SEED_DB) and the interned tree as the
#     source override — so /var/guix and the live /gnu/store TOOLCHAIN paths are out of the
#     build's input path. If any seed path were missing the build would fail (no guix
#     fallback), so a green build proves the rust-bearing seed is self-sufficient.
#
# guix/Guile are SCRUBBED FROM PATH (no guix process); guix appears only as the one-time
# capture SOURCE (the seed comes from it once) + the removable equivalence oracle.
#
# Legs (differential + durable discipline):
#   [DURABLE structural]  the build stages toolchain inputs FROM the unpacked seed (closure
#                         binds on-disk under DEST-STORE, none bare-/gnu/store); the .drv
#                         builder is the td-bootstrapped stage0.
#   [DURABLE behavioral]  the seed-built td-builder RUNS (nar-hash) and agrees with the
#                         stage0 td-builder (both td-built — no guix-built oracle, so the
#                         guix-surface ratchet does not grow).
#   [DURABLE repro]       td-builder check's double-build agrees the output is reproducible.
#   [REMOVABLE oracle]    the seed-built td-builder == the guix-seed build (same drv —
#                         own, then diverge: provenance changed, output didn't).
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }
root=$(pwd)

export 

. tests/cache-lib.sh
export TD_STAGE0_BASE="$root/.td-build-cache/stage0"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
load_recipe_eval || fail "no td-built td-recipe-eval (the build-recipes prelude must run first)"
case "$TD_RECIPE_EVAL" in *.td-build-cache/*) : ;; *) fail "TD_RECIPE_EVAL is not td's own build ($TD_RECIPE_EVAL)" ;; esac
echo ">> td tools (guix-free): stage0=$TB  ts-eval=$TD_RECIPE_EVAL"

lock0="$root/tests/td-builder-rust.lock"
test -s "$lock0" || fail "no rust lock $lock0"
cu=`grep -- '-coreutils-' "$lock0" | sed 's/^[^ ]* //' | head -1`
test -n "$cu" || fail "no coreutils in the rust lock for the scrubbed PATH"
if ls "$cu/bin" | grep -qE '^(guix|guile)$'; then fail "guix/guile on the scrubbed PATH"; fi

scratch="$root/.td-build-cache/rust-seed"; rm -rf "$scratch"; mkdir -p "$scratch/tmp" "$scratch/b"
work=`mktemp -d`
trap 'chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"' EXIT INT TERM

# --- Intern the LIVE builder tree (td's own store-add-recursive, no guix repl) ---------
srcinfo=`sh tests/intern-src.sh "$TB" td-builder-src "$root/builder" "$scratch" target .cargo` \
  || fail "td could not intern the current builder tree (store-add-recursive)"
eval "$srcinfo"
test -n "$src" -a -d "$srcstore/`basename "$src"`" || fail "td interned no source tree"
lock="$scratch/td-builder-rust.lock"; { cat "$lock0"; echo "td-builder-source $src"; } > "$lock"
echo ">> interned the CURRENT builder tree (recursive addToStore, no guix repl / no daemon): $src"

# --- WARM the RUST TOOLCHAIN seed (capture + unpack ONCE into a reusable content-addressed
#     cache — the #135 warm-seed rail; no 2GB re-capture per run). Roots: the rust lock's
#     toolchain store paths (rust/cargo/gcc-toolchain/coreutils/bash) + the stage0 builder's
#     runtime refs (so the seed covers the in-sandbox builder too).
grep ' /gnu/store/' "$lock0" | sed 's/^[^ ]* //' | sort -u > "$work/roots"
"$TB" store-query "$TD_BUILDER_DB" references 2>/dev/null | sed 's/^[^|]*|//' | grep '^/gnu/store/' >> "$work/roots" || true
sort -u "$work/roots" -o "$work/roots"
xargs guix build < "$work/roots" >/dev/null || fail "could not realize the rust toolchain seed closure"
seedline=`TB="$TB" TD_SEED_DB=/var/guix/db/db.sqlite sh tools/warm-seed.sh "$root/.td-build-cache/seed" $(cat "$work/roots")` \
  || fail "warm-seed (rust toolchain) failed"
SEED_STORE=`echo "$seedline" | cut -d' ' -f1`
SEED_DB=`echo "$seedline" | cut -d' ' -f2`
SEED_MANIFEST=`echo "$seedline" | cut -d' ' -f3`
test -d "$SEED_STORE" -a -s "$SEED_DB" -a -s "$SEED_MANIFEST" || fail "warm-seed produced no usable rust seed"
ns=`grep -c . "$SEED_MANIFEST"`
echo "   warmed the RUST TOOLCHAIN seed: $ns paths, cached at $SEED_STORE (no per-run re-capture)"

# --- ts-emit the td-builder recipe (buildSystem rust), Guile-free ---------------------
sh tests/recipe-emit.sh td-builder > "$scratch/td-builder.json" || fail "ts-emit td-builder"
grep -q '"buildSystem":"rust"' "$scratch/td-builder.json" || fail "recipe JSON is not buildSystem rust"

# --- Build td-builder from the UNPACKED SEED ONLY (toolchain) + the interned source ----
# TD_SEED_STORE/TD_SEED_DB: the input closure comes from the seed; toolchain inputs bind
# from the unpacked store. SRC-STORE/SRC-DB: the live source override (not frozen). The
# positional STORE-DB is the seed db. guix/Guile off PATH.
sd="$scratch/b"
env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$cu/bin" \
  TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
  TD_SEED_STORE="$SEED_STORE" TD_SEED_DB="$SEED_DB" \
  "$TB" build-recipe "$scratch/td-builder.json" "$lock" "$sd" "$SEED_DB" "$srcstore" "$srcdb" \
  > "$scratch/bout" 2>"$scratch/err" \
  || { tail -30 "$scratch/err" >&2; fail "build td-builder from the unpacked RUST SEED FAILED (seed not self-sufficient?)"; }
out=`sed -n 's/^OUT=out //p' "$scratch/bout"`
test -n "$out" || { cat "$scratch/err" >&2; fail "build-recipe produced no output"; }
nsd="$sd/newstore/`basename "$out"`"
test -x "$nsd/bin/td-builder" || fail "seed-built td-builder missing at $nsd/bin/td-builder"

# --- Leg A: DURABLE structural — staged from the seed, builder is stage0 ---------------
test -s "$sd/closure.txt" || fail "no closure.txt from the build"
grep -q "	$SEED_STORE/" "$sd/closure.txt" \
  || fail "the build staged no input from the warmed seed store ($SEED_STORE)"
bare=`grep -v '	' "$sd/closure.txt" | grep '^/gnu/store/' | grep -v "^$out$" | head -1 || true`
test -z "$bare" || fail "an input staged from the live /gnu/store, not the seed: $bare"
test -n "$TD_BUILDER_PATH" || fail "TD_BUILDER_PATH unset — load_stage0 did not place a stage0 builder"
grep -qF "$TD_BUILDER_PATH/bin/td-builder" "$sd"/*.drv \
  || fail "the .drv builder is not the stage0 $TD_BUILDER_PATH — built by the wrong td-builder?"
echo "   [DURABLE structural] every toolchain input staged from the unpacked RUST seed (none bare /gnu/store); the .drv builder is the stage0 ($TD_BUILDER_PATH)"

# --- Leg B: DURABLE behavioral — the seed-built td-builder RUNS + matches stage0 -------
# Compare to the stage0 td-builder (TB), NOT the guix-built one: both are td's own
# binaries, so this is a guix-free behavioral-equivalence check (no `guix build -e`
# packager site — the guix-surface ratchet must not grow). Same source + same nar-hash
# algorithm ⇒ identical probe hash; a mismatch means the seed-built engine misbehaves.
printf 'td rust-seed behavioral probe\n' > "$scratch/probe"
h_td=`"$nsd/bin/td-builder" nar-hash "$scratch/probe"`
test -n "$h_td" || fail "the seed-built td-builder did not run / produced no nar-hash"
h_s0=`"$TB" nar-hash "$scratch/probe"`
echo "   [DURABLE behavioral] the seed-built td-builder RUNS: nar-hash = $h_td"
test "$h_td" = "$h_s0" || fail "seed-built and stage0 td-builder disagree ($h_td != $h_s0)"
echo "   [DURABLE behavioral] it agrees with the stage0 td-builder (behavioral equivalence, both td-built — no guix)"

# --- Leg C: DURABLE repro — td-builder check double-build ------------------------------
rm -rf "$scratch/chk"
"$TB" check-drv "$sd"/*.drv "$sd/closure.txt" "$scratch/chk" > "$scratch/checkout.txt" 2>"$scratch/chk.err" \
  || { cat "$scratch/checkout.txt" "$scratch/chk.err" >&2; fail "rust-seed NOT reproducible (td-builder check)"; }
grep -qE "^CHECK out $out sha256:[0-9a-f]+ reproducible$" "$scratch/checkout.txt" \
  || { cat "$scratch/checkout.txt" >&2; fail "td-builder check did not confirm $out reproducible"; }
echo "   [DURABLE repro] td-builder check double-build agrees the seed-built td-builder is reproducible"

# --- Leg D: REMOVABLE oracle — same store path as the guix-seed build ------------------
mkdir -p "$scratch/g"
env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$cu/bin" \
  TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
  "$TB" build-recipe "$scratch/td-builder.json" "$lock" "$scratch/g" /gnu/store "$srcstore" "$srcdb" \
  > "$scratch/gout" 2>"$scratch/gerr" || { tail -10 "$scratch/gerr" >&2; fail "guix-seed build (oracle) failed"; }
gout=`sed -n 's/^OUT=out //p' "$scratch/gout"`
test "$out" = "$gout" || fail "seed-built td-builder ($out) != guix-seed build ($gout) — provenance changed the output"
echo "   [REMOVABLE oracle] seed-built td-builder == guix-seed build (`basename "$out"`) — same drv, provenance-only change"

echo "PASS: td built its own Rust build engine (td-builder) entirely from the UNPACKED RUST SEED"
echo "      (toolchain store DB = the seed only) — /var/guix + the live /gnu/store toolchain"
echo "      out of the build path; the seed-built td-builder runs, agrees with guix's, and is"
echo "      reproducible. RUST IS IN THE SEED: td builds its engine with no guix install."
