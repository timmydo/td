#!/bin/sh
# tests/td-shell-seed.sh — North-Star: `td shell` is FULLY guix-free. Step 1's td-shell
# gate proved `td shell hello -- hello` builds td's hello with no guix PROCESS (guix off
# PATH). This gate closes the loop: with a warmed seed (TD_SEED_STORE/TD_SEED_DB) td shell
# builds hello from the frozen seed as its ONLY store DB — so /var/guix and the live
# /gnu/store are out of the build too. No code change: `run_shell` spawns `build-recipe`,
# which inherits TD_SEED_* and uses the seed-store override (#133). So the user-facing
# command builds td's own package with NO guix at all — no process, no install.
#
# The seed is the warmed, content-addressed cache (tools/warm-seed.sh) — captured by
# CONTENT-SCANNING the store bytes (seed-manifest over the store dir; no /var/guix/db read);
# guix/Guile are SCRUBBED FROM PATH (proving no guix process); the only guix left is the
# guix-BUILT seed bytes (the toolchain seed, retired last) + the removable equivalence oracle.
#
# Legs:
#   [DURABLE behavioral] `td shell hello -- hello` builds + runs from the seed (guix off
#                        PATH, store DB = the seed only) → Hello, world!
#   [DURABLE structural] the build staged inputs FROM the warmed seed store (none from the
#                        live /gnu/store); the hello on PATH is td's build
#   [REMOVABLE oracle]   the seed-built hello is the SAME store path as the guix build
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }

. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
load_recipe_eval || fail "no td-built td-recipe-eval (the build-recipes prelude must run first)"
echo ">> td tools (guix-free): stage0=$TB  recipe-eval=$TD_RECIPE_EVAL"

work=`mktemp -d`
trap 'chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"' EXIT INT TERM
# coreutils + bash are DECLARED gate inputs (#353): resolved by the runner.
cu=${TD_GATE_INPUT_COREUTILS:-}
sh_=${TD_GATE_INPUT_BASH:-}
test -n "$cu" -a -n "$sh_" || { echo "ERROR: TD_GATE_INPUT_{COREUTILS,BASH} unset — run via td-builder gate-run, which resolves the gate's declared inputs" >&2; exit 1; }
test -n "$cu" -a -n "$sh_" || fail "no coreutils/bash in hello lock"
if ls "$cu/bin" "$sh_/bin" | grep -qE '^(guix|guile)$'; then fail "guix/guile on the scrubbed PATH"; fi

# WARM hello's seed (same roots the seed-build gate uses): lock inputs + stage0 runtime.
grep ' /gnu/store/' tests/hello-no-guix.lock | sed 's/^[^ ]* //' | sort -u > "$work/roots"
"$TB" store-query "$TD_BUILDER_DB" references 2>/dev/null | sed 's/^[^|]*|//' | grep '^/gnu/store/' >> "$work/roots" || true
sort -u "$work/roots" -o "$work/roots"
grep ' /gnu/store/' tests/hello-no-guix.lock | sed 's/^[^ ]* //' | sort -u | xargs guix build >/dev/null \
  || fail "could not realize hello's seed closure"
# Capture the seed by CONTENT-SCANNING the store dir (TD_SEED_DB=/gnu/store) — seed-manifest
# computes the closure + every member's refs from the store bytes (== guix gc -R; gate 290),
# with NO read of guix's private /var/guix/db. The captured seed is byte-identical to a
# DB-read capture (same closure, NAR hashes, refs), so the seed-built hello is unchanged.
seedline=`TB="$TB" TD_SEED_DB=/gnu/store sh tools/warm-seed.sh "$(pwd)/.td-build-cache/seed" $(cat "$work/roots")` \
  || fail "warm-seed failed"
SEED_STORE=`echo "$seedline" | cut -d' ' -f1`; SEED_DB=`echo "$seedline" | cut -d' ' -f2`
test -d "$SEED_STORE" -a -s "$SEED_DB" || fail "warm-seed produced no usable seed"
echo "   warmed seed: $SEED_STORE"

# td shell, run with guix/Guile OFF PATH and the SEED as the build's only store DB.
tdshell() {
  env -i HOME="$work" TMPDIR="$work/tmp" PATH="$cu/bin:$sh_/bin" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    TD_RECIPE_EVAL="$TD_RECIPE_EVAL" \
    TD_SHELL_LOCKS=tests TD_SHELL_CACHE="$work/pkgs" \
    TD_SEED_STORE="$SEED_STORE" TD_SEED_DB="$SEED_DB" \
    "$TB" shell "$@"
}
mkdir -p "$work/tmp"

# --- Leg A: DURABLE behavioral — build + run hello from the seed, no guix at all ---
echo ">> [DURABLE behavioral] td shell hello -- hello (guix OFF PATH, store DB = the seed)"
out=`tdshell hello -- hello 2>"$work/a.err"` || { tail -20 "$work/a.err" >&2; fail "td shell hello (from seed) failed"; }
test "$out" = "Hello, world!" || fail "td shell hello printed '$out'"
echo "   ok: hello BUILT from the seed (no guix process, no /var/guix) and greeted"

# --- Leg B: DURABLE structural — staged from the seed; hello on PATH is td's build ---
hb=`tdshell hello -- bash -c 'command -v hello'` || fail "could not locate hello on PATH"
case "$hb" in "$work"/pkgs/hello/newstore/*-hello-*/bin/hello) : ;; *) fail "hello on PATH is '$hb', not td-built under the cache" ;; esac
test -s "$work/pkgs/hello/closure.txt" || fail "no build closure.txt"
grep -q "	$SEED_STORE/" "$work/pkgs/hello/closure.txt" || fail "the build did not stage any input from the seed store"
outbase=`basename "$(dirname "$(dirname "$hb")")"`
bare=`grep -v '	' "$work/pkgs/hello/closure.txt" | grep '^/gnu/store/' | grep -v "/$outbase\$" | head -1 || true`
test -z "$bare" || fail "an input staged from the live /gnu/store, not the seed: $bare"
echo "   [DURABLE structural] PATH hello = $hb (td's build); every input staged FROM the seed store"

# --- Leg C: REMOVABLE oracle — same td path whether hello is built from the frozen SEED or
# CONTENT-SCANNED from the LIVE /gnu/store (provenance-only change; td's own build path is
# distinct from guix's daemon hello — own, then diverge — so the oracle is td's own live-store
# build, NOT `guix build`). TD_SHELL_STORE_DB is the live store dir the build scans for hello's
# closure (== guix gc -R) — no /var/guix read.
gxout=`env -i HOME="$work" TMPDIR="$work/tmp" PATH="$cu/bin:$sh_/bin" \
  TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
  TD_RECIPE_EVAL="$TD_RECIPE_EVAL" \
  TD_SHELL_LOCKS=tests TD_SHELL_CACHE="$work/pkgs-guix" TD_SHELL_STORE_DB=/gnu/store \
  "$TB" shell hello -- bash -c 'command -v hello'` || fail "td shell hello via the live store (oracle) failed"
gxbase=`basename "$(dirname "$(dirname "$gxout")")"`
test "$outbase" = "$gxbase" || fail "seed-built hello ($outbase) != live-store-built td shell ($gxbase)"
echo "   [REMOVABLE oracle] seed-built hello == live-store-built td shell ($gxbase) — provenance-only change"

echo "PASS: td shell builds + runs td's OWN hello entirely from the frozen seed — guix/Guile"
echo "      off PATH (no process) AND the seed as the only store DB (no /var/guix). The"
echo "      user-facing command is fully guix-free: no guix process, no guix install."
