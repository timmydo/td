#!/bin/sh
# tests/make-x86-64-recipe-check.sh — the recipe-owned RecipeCheck::daily body for make-x86-64
# (issue #388 rung 1: the FIRST td-native build-userland tool). Drives the recipe-graph model:
#
#   [supply-chain]        the pinned GNU make 4.4.1 tarball matches its sha256 pin (the sha IS the
#                         oracle — td-fetched upstream, no guix origin).
#   [recipe-graph-wiring] the make-x86-64 recipe DECLARES the native /td/store toolchain
#                         (gcc-x86-64-native + binutils-x86-64-native + glibc-x86-64) as its
#                         nativeInputs — i.e. it builds ON td packages from the mes-rooted chain,
#                         NOT on guix-built build tools (the #388 re-aim). Cheap (emit only), so it
#                         runs — and verified-red — on a dev box before the heavy build.
#   [recipe-graph]        `build-plan --auto make-x86-64` builds the /td/store make over that native
#                         toolchain (no committed/gate-assembled lock; the ladder chains the graph).
#                         The output is content-addressed at /td/store.
#   [no-guix]             the built make tree carries zero /gnu/store bytes.
#   [behavioral]          make 4.4.1 RUNS from /td/store in a store-ns own-root (/gnu/store ABSENT),
#                         reports 4.4.1, AND drives a one-rule build (the actual feature — a static
#                         make needs no glibc staging in the own-root).
#
# HEAVY: build-plan --auto make-x86-64 realizes the x86_64 native toolchain (warm chain cache-hits the
# toolchain rungs; cold from-seed is memory-heavy, #371). Runs via the recipe-checks-daily gate
# (Pool::Daily), which resolves TD_GATE_INPUT_{COREUTILS,BASH_STATIC}. Deferred to the daily backstop —
# same posture as the rust-toolchain recipe check it is modeled on.
#
# Reproducibility (directive-3 callout, see PR): the make build is deterministic-by-construction
# (fixed content-addressed inputs + a from-source compile), so — like the mesboot rungs it chains over —
# its byte-for-byte double-build is the daily force-cold from-seed run (the content-addressed store
# enforces byte-identity), NOT a warm per-run double-build. This check asserts the content-addressed
# structural signal + the behavioral run; the daily backstop is the authoritative double-build.
set -eu

ROOT=$(pwd)
fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
lf() { sed -n "s/^$2 //p" "$1" | head -1; }

# --- [supply-chain] the pinned GNU make 4.4.1 tarball matches its sha256 pin -------------------
MK_LOCK=$(ls seed/sources/make-4.4.1*.lock 2>/dev/null | head -1)
test -n "$MK_LOCK" || fail "no seed/sources/make-4.4.1*.lock"
MK_TB=".td-build-cache/sources/$(lf "$MK_LOCK" file)"
test -f "$MK_TB" || fail "the pinned make 4.4.1 tarball is not warm ($MK_TB) — run 'td-feed warm sources'"
test "$(sha "$MK_TB")" = "$(lf "$MK_LOCK" sha256)" || fail "warmed $MK_TB sha256 != lock pin"
echo "   [supply-chain] the pinned GNU make 4.4.1 tarball matches its sha256 pin (the sha is the oracle)"

# --- stage0 builder + recipe evaluator + curated PATH -----------------------------------------
. tests/cache-lib.sh
. tests/x86_64-cross-fns.sh
CU=${TD_GATE_INPUT_COREUTILS:-}
test -n "$CU" || fail "TD_GATE_INPUT_COREUTILS unset — run via td-builder gate-run (recipe-checks-daily)"
export TD_STAGE0_BASE="$PWD/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
load_recipe_eval || fail "no td-recipe-eval"
export TD_STORE_DIR=/td/store

# --- [recipe-graph-wiring] the recipe builds ON the native /td/store toolchain (#388 re-aim) ---
# Cheap: `emit` only, no build. Each of the three native rungs appears solely in the recipe's
# nativeInputs, so a plain grep of the emitted JSON is a faithful assertion — and reds if a future
# edit drops the native toolchain (the whole point of the rung).
for ni in gcc-x86-64-native binutils-x86-64-native glibc-x86-64; do
  "$TD_RECIPE_EVAL" emit make-x86-64 | grep -q "\"$ni\"" \
    || fail "the make-x86-64 recipe does not declare native input '$ni' — rung 1 must build ON the native /td/store toolchain, not guix build tools (#388)"
done
echo "   [recipe-graph-wiring] the make-x86-64 recipe declares the native /td/store toolchain (gcc-x86-64-native + binutils-x86-64-native + glibc-x86-64) as its build compiler"

# --- [recipe-graph] build the /td/store make via build-plan --auto ----------------------------
run_x86_64_make || fail "build-plan --auto make-x86-64 failed"
test -n "${XMAKE:-}" -a -x "$XMAKE/bin/make" || fail "the make-x86-64 ladder produced no make binary"
case "$XMAKE" in */[a-z0-9]*-make-x86-64-4.4.1) ;; *) fail "make-x86-64 output is not content-addressed (got $XMAKE)" ;; esac
echo "   [recipe-graph] build-plan --auto make-x86-64 built the content-addressed tree at $XMAKE"

# --- [no-guix] the interned make tree carries zero /gnu/store bytes ----------------------------
if grep -r -a -q '/gnu/store' "$XMAKE" 2>/dev/null; then
  fail "the interned make tree contains /gnu/store bytes: $(grep -r -a -l '/gnu/store' "$XMAKE" 2>/dev/null | head -1)"
fi
echo "   [no-guix] the built make 4.4.1 tree carries zero /gnu/store bytes"

# --- [behavioral] make RUNS from /td/store in a store-ns own-root and DRIVES a build ----------
snwork=$(mktemp -d)
trap 'chmod -R u+w "$snwork" 2>/dev/null || true; rm -rf "$snwork"' EXIT INT TERM
store="$snwork/store"; mkdir -p "$store"
# a STATIC make has no interp — stage ONLY the make tree + the static bash (no glibc co-location
# needed, unlike the dynamic rust-toolchain probe). make's recipe SHELL is the static bash, whose
# builtins (echo/read/printf/[/cd) run the one-rule build with no coreutils in the own-root.
makebase=$(basename "$XMAKE")
cp -a "$XMAKE" "$store/$makebase"
bs=${TD_GATE_INPUT_BASH_STATIC:-}
test -n "$bs" -a -x "$bs/bin/bash" || fail "TD_GATE_INPUT_BASH_STATIC unset/invalid — the gate must declare bash-static"
bbase=$(basename "$bs"); cp -a "$bs" "$store/$bbase"; chmod -R u+w "$store"
cat > "$store/probe.sh" <<PROBE
[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
/td/store/$makebase/bin/make --version && echo MAKE-VERSION-OK
cd /tmp || exit 1
printf 'all:\n\techo BUILT > out.txt\n' > Makefile
/td/store/$makebase/bin/make SHELL=/td/store/$bbase/bin/bash >/dev/null 2>&1 || { echo MAKE-DRIVE-FAIL; exit 1; }
read line < out.txt
[ "\$line" = BUILT ] && echo MAKE-DROVE-A-BUILD
PROBE
out=$("$TB" store-ns "$store" -- "/td/store/$bbase/bin/bash" /td/store/probe.sh 2>&1) \
  || { printf '%s\n' "$out" | sed 's/^/     /' >&2; fail "store-ns make probe exited nonzero"; }
printf '%s\n' "$out" | sed 's/^/     /'
printf '%s\n' "$out" | grep -q '^GNU-ABSENT$'        || fail "/gnu/store is PRESENT in the own-root"
printf '%s\n' "$out" | grep -q '^GNU Make 4\.4\.1'   || fail "make did not report 4.4.1 from /td/store"
printf '%s\n' "$out" | grep -q '^MAKE-VERSION-OK$'   || fail "make --version did not run cleanly from /td/store"
printf '%s\n' "$out" | grep -q '^MAKE-DROVE-A-BUILD$' || fail "make did not drive a one-rule build from /td/store"
echo "   [behavioral] make 4.4.1 RUNS from /td/store in the own-root (/gnu/store ABSENT), reports 4.4.1, and drives a build"

echo "PASS: make-x86-64 recipe check — build-plan --auto builds GNU make 4.4.1 on the NATIVE /td/store"
echo "  x86_64 toolchain (gcc-x86-64-native + binutils-x86-64-native + glibc-x86-64; recipe-graph, no"
echo "  gate-assembled lock); the static make RUNS from /td/store in a /gnu/store-absent own-root (4.4.1)"
echo "  and drives a build — the first td-native build-userland tool. Byte-for-byte double-build: the"
echo "  daily force-cold backstop (see the note above)."
