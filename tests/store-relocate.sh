#!/bin/sh
# tests/store-relocate.sh — user-pm Phase 2: relocate a DYNAMIC package's closure from
# guix's /gnu/store to td's /td/store and run it there, with NO /gnu/store. The break from
# guix made real for dynamic binaries. `td-builder store-relocate STORE-DB ROOT DEST` copies
# ROOT's closure into DEST and rewrites every /gnu/store reference to /td//store — the
# length-preserving (10→10 bytes), kernel-collapsed form of /td/store, so RUNPATH/interpreter
# in .dynstr, embedded paths in .rodata, and scripts are all handled by one binary-safe byte
# substitution (no patchelf, no ELF surgery). DEST bound at /td/store (store-ns) IS the
# relocated store; the binary's /td//store refs resolve into it.
#
# This gate relocates hello's closure (hello + glibc + gcc-lib) and runs hello inside the
# store-ns: it greets with /gnu/store ABSENT, the relocated binary has NO /gnu/store left, and
# the run matches guix's hello (removable oracle). guix is only the one-time relocation SOURCE
# (the seed is captured from it once). td-builder is the guix-free stage0.
#
# Legs:
#   [DURABLE behavioral] the relocated dynamic binary runs from /td/store (no /gnu/store)
#   [DURABLE structural] the relocated binary has NO /gnu/store refs (all → /td//store)
#   [REMOVABLE oracle]   it greets identically to guix's /gnu/store hello
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }

. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
echo ">> td-builder under test (stage0, guix-free): $TB"

work=`mktemp -d`
trap 'chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"' EXIT INT TERM

hello=`guix build hello 2>/dev/null` || fail "guix build hello (the relocation source)"
hbase=`basename "$hello"`

# RELOCATE hello's closure /gnu/store -> /td/store (td's own store-relocate).
"$TB" store-relocate /var/guix/db/db.sqlite "$hello" "$work/store" >/dev/null \
  || fail "store-relocate failed"
test -x "$work/store/$hbase/bin/hello" || fail "relocated hello binary missing"

# --- Leg A: DURABLE behavioral — run the relocated DYNAMIC binary from /td/store ----
out=`"$TB" store-ns "$work/store" -- "/td/store/$hbase/bin/hello"` \
  || fail "relocated hello did not run in the store-ns"
test "$out" = "Hello, world!" || fail "relocated hello printed '$out'"
echo "   [DURABLE behavioral] the relocated dynamic hello ran from /td/store (no /gnu/store) and greeted"

# --- Leg B: DURABLE structural — no /gnu/store left; rewritten to /td//store --------
if grep -q -a '/gnu/store' "$work/store/$hbase/bin/hello"; then
  fail "the relocated hello binary STILL has /gnu/store references"
fi
grep -q -a '/td//store' "$work/store/$hbase/bin/hello" \
  || fail "the relocated hello binary has no /td//store references (was it rewritten?)"
echo "   [DURABLE structural] the relocated binary has NO /gnu/store refs — all rewritten to /td//store"

# --- Leg C: REMOVABLE oracle — same greeting as guix's /gnu/store hello -------------
test "$out" = "`"$hello/bin/hello"`" || fail "relocated hello != guix hello"
echo "   [REMOVABLE oracle] the relocated hello greets identically to guix's /gnu/store hello"

echo "PASS: td relocated a dynamic package's closure from guix's /gnu/store to /td/store"
echo "      (size-preserving /gnu/store -> /td//store rewrite, no patchelf) and ran it from"
echo "      /td/store with /gnu/store ABSENT — the break from guix, made real for dynamic"
echo "      binaries (user-pm Phase 2)."
