#!/bin/sh
# tests/rust-toolchain-recipe-check.sh — the recipe-owned RecipeCheck::daily body for
# rust-toolchain (#410). Drives the recipe-graph model end to end:
#
#   [supply-chain] the pinned upstream Rust 1.96.0 release tarball matches its sha (the sha
#                  IS the oracle — no guix origin, no differential).
#   [recipe-graph] `build-plan --auto rust-toolchain` builds the /td/store rust toolchain over
#                  glibc-x86-64 + gcc-x86-64-stage2 + zlib-x86-64 (no committed/gate-assembled
#                  lock; the ladder chains the graph). The relinked rustc/cargo tree is
#                  content-addressed at /td/store (deterministic-by-construction).
#   [behavioral]   rustc + cargo RUN from /td/store in a store-ns own-root, reporting 1.96.0,
#                  with /gnu/store ABSENT (the interp is the /td/store x86_64 glibc loader; the
#                  runtime closure — libc/libgcc_s/libz/librustc_driver/libLLVM — is co-located).
#   [verified-red] the transform (`td-builder rust-toolchain-build`) REDS naming the missing
#                  input when a declared input is absent from TD_INPUT_MAP.
#
# Reproducibility note (directive-3 callout, see PR): the transform is deterministic-by-
# construction (fixed content-addressed inputs + fs copies + crate::elf relink), so — like the
# mesboot rungs it chains over (x86_64-cross-fns.sh:"the recipe now has the SAME reproducibility
# treatment as every recipe rung — deterministic-by-construction plus the daily force-cold
# backstop") — its byte-for-byte double-build is the daily force-cold from-seed run (the
# content-addressed store enforces byte-identity: a non-reproducible transform lands at a
# different /td/store hash and breaks the chain), NOT a warm per-run double-build (which the
# content-addressed cache would trivially satisfy). This check asserts the content-addressed
# structural signal + the behavioral run; the daily backstop is the authoritative double-build.
#
# HEAVY: build-plan --auto rust-toolchain realizes the x86_64 toolchain (warm chain cache-hits
# the cross rungs; cold from-seed is ~98 min + memory-heavy, #371). Runs via the
# recipe-checks-daily gate (Pool::Daily), which resolves TD_GATE_INPUT_{COREUTILS,BASH_STATIC}.
set -eu

ROOT=$(pwd)
fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
lf() { sed -n "s/^$2 //p" "$1" | head -1; }

# --- [supply-chain] the pinned Rust release tarball matches its sha256 pin --------------------
RUST_LOCK=$(ls seed/sources/rust-1.96.0*.lock 2>/dev/null | head -1)
test -n "$RUST_LOCK" || fail "no seed/sources/rust-1.96.0*.lock"
RUST_TB=".td-build-cache/sources/$(lf "$RUST_LOCK" file)"
test -f "$RUST_TB" || fail "the pinned Rust release tarball is not warm ($RUST_TB) — run 'td-feed warm sources'"
test "$(sha "$RUST_TB")" = "$(lf "$RUST_LOCK" sha256)" || fail "warmed $RUST_TB sha256 != lock pin"
echo "   [supply-chain] the pinned Rust 1.96.0 release tarball matches its sha256 pin (the sha is the oracle)"

# --- stage0 builder + recipe evaluator + curated (compiler-free) PATH -------------------------
. tests/cache-lib.sh
. tests/x86_64-cross-fns.sh
CU=${TD_GATE_INPUT_COREUTILS:-}
test -n "$CU" || fail "TD_GATE_INPUT_COREUTILS unset — run via td-builder gate-run (recipe-checks-daily)"
export TD_STAGE0_BASE="$PWD/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
load_recipe_eval || fail "no td-recipe-eval"
export TD_STORE_DIR=/td/store

# --- [recipe-graph] build the /td/store rust toolchain via build-plan --auto ------------------
run_x86_64_rust_toolchain || fail "build-plan --auto rust-toolchain failed"
test -n "${XRUSTTREE:-}" -a -x "$XRUSTTREE/bin/rustc" -a -x "$XRUSTTREE/bin/cargo" \
  || fail "the rust-toolchain ladder produced no rustc/cargo tree"
case "$XRUSTTREE" in */[a-z0-9]*-rust-toolchain-1.96.0) ;; *) fail "rust-toolchain output is not content-addressed (got $XRUSTTREE)" ;; esac
echo "   [recipe-graph] build-plan --auto rust-toolchain built the relinked, content-addressed tree at $XRUSTTREE"

# the interned rust deliverable carries NO guix bytes (upstream-release bytes are not guix bytes).
for b in "$XRUSTTREE/bin/rustc" "$XRUSTTREE/bin/cargo"; do
  if grep -q -a '/gnu/store' "$b"; then fail "$b contains /gnu/store bytes"; fi
done
echo "   [no-guix] the relinked rustc/cargo carry zero /gnu/store bytes"

# --- [behavioral] rustc + cargo RUN from /td/store in a store-ns own-root, /gnu/store ABSENT ---
snwork=$(mktemp -d)
trap 'chmod -R u+w "$snwork" 2>/dev/null || true; rm -rf "$snwork"' EXIT INT TERM
store="$snwork/store"; mkdir -p "$store"
# stage the rust + glibc RUNG OUTPUTS at their canonical basenames (build-plan staged them under
# $XRUSTTREE / XGLIBC's rung dir): rustc's interp points at /td/store/<glibc-rung-base>/stage/…,
# and RUNPATH ($ORIGIN/../lib) at /td/store/<rust-rung-base>/lib — both must resolve in the ns.
rustbase=$(basename "$XRUSTTREE")
glibcout=${XGLIBC%/stage/td/store/glibc-2.41-x86_64}
glibcbase=$(basename "$glibcout")
cp -a "$XRUSTTREE" "$store/$rustbase"
test -e "$store/$glibcbase" || cp -a "$glibcout" "$store/$glibcbase"
bs=${TD_GATE_INPUT_BASH_STATIC:-}
test -n "$bs" -a -x "$bs/bin/bash" || fail "TD_GATE_INPUT_BASH_STATIC unset/invalid — the gate must declare bash-static"
bbase=$(basename "$bs"); cp -a "$bs" "$store/$bbase"; chmod -R u+w "$store"
cat > "$store/probe.sh" <<PROBE
[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
/td/store/$rustbase/bin/rustc -vV && echo RUSTC-VV-OK
/td/store/$rustbase/bin/cargo --version && echo CARGO-OK
PROBE
out=$("$TB" store-ns "$store" -- "/td/store/$bbase/bin/bash" /td/store/probe.sh 2>&1) \
  || { printf '%s\n' "$out" | sed 's/^/     /' >&2; fail "store-ns rustc/cargo probe exited nonzero"; }
printf '%s\n' "$out" | sed 's/^/     /'
printf '%s\n' "$out" | grep -q '^GNU-ABSENT$'  || fail "/gnu/store is PRESENT in the own-root"
printf '%s\n' "$out" | grep -q '^rustc 1\.96\.0' || fail "rustc did not report 1.96.0 from /td/store"
printf '%s\n' "$out" | grep -q '^RUSTC-VV-OK$'  || fail "rustc -vV did not run cleanly from /td/store"
printf '%s\n' "$out" | grep -q '^cargo 1\.96\.0' || fail "cargo did not report 1.96.0 from /td/store"
printf '%s\n' "$out" | grep -q '^CARGO-OK$'     || fail "cargo --version did not run cleanly from /td/store"
echo "   [behavioral] rustc + cargo RUN from /td/store in the own-root (/gnu/store ABSENT), reporting 1.96.0"

# --- [verified-red] the transform reds naming a declared input absent from TD_INPUT_MAP --------
# from_drv_env picks glibc-x86-64 FIRST, so a map without it reds at env parse (before any build).
badmap='{"gcc-x86-64-stage2":"/nonexistent","zlib-x86-64":"/nonexistent","tar":"/nonexistent","gzip":"/nonexistent"}'
if env TD_SRC="$RUST_TB" TD_INPUT_MAP="$badmap" out="$snwork/vr" "$TB" rust-toolchain-build 2>"$snwork/vr.err"; then
  fail "verified-red: the transform SUCCEEDED with glibc-x86-64 absent from TD_INPUT_MAP"
fi
grep -q 'glibc-x86-64' "$snwork/vr.err" || { cat "$snwork/vr.err" >&2; fail "verified-red: the transform red did not name glibc-x86-64"; }
echo "   [verified-red] the transform reds naming glibc-x86-64 when its input is absent from TD_INPUT_MAP"

echo "PASS: rust-toolchain recipe check — build-plan --auto builds the /td/store rust toolchain over"
echo "  glibc-x86-64 + gcc-x86-64-stage2 + zlib-x86-64 (recipe-graph, no gate-assembled lock); rustc +"
echo "  cargo RUN from /td/store in a /gnu/store-absent own-root (1.96.0); a missing declared input reds"
echo "  the transform. Byte-for-byte double-build: the daily force-cold backstop (see the note above)."
