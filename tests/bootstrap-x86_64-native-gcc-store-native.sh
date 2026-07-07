#!/bin/sh
# tests/bootstrap-x86_64-native-gcc-store-native.sh — source-bootstrap: rung X2 of the x86_64-toolchain
# track. Rung X1 (#201) crossed the i686 full-source bootstrap UP to a CROSS x86_64 toolchain — a cross
# gcc 14.3.0 that is itself an i686 (ELF 32-bit) binary EMITTING x86_64 + the MODERN x86_64 glibc 2.41.
# Rung X2 turns that into a NATIVE x86_64 gcc: with the cross toolchain (XGCC2/XBU) td builds NATIVE
# x86_64 binutils 2.44 + NATIVE x86_64 GCC 14.3.0 (gcc/cc1/g++ that are themselves ELF 64-bit x86_64,
# --build=--host=--target=x86_64), STATIC vs the /td/store x86_64 glibc 2.41. The native gcc is interned
# at /td/store and RUN in the store-ns own-root, where it COMPILES a C and a C++ program from source and
# both run → 42, /gnu/store ABSENT — the compiler doing the work is itself an x86_64 binary in /td/store.
# This is the architectural self-hosting rung (the toolchain's host arch == its target arch); a
# from-source gcc-rebuilds-gcc bootstrap is a separate, much heavier milestone and is NOT claimed here.
#
# The cross toolchain prerequisite is obtained EXACTLY like the x86_64 gate (414): FETCH the lock-keyed
# signed closure {binutils-2.44-x86_64, gcc-14.3.0-x86_64, glibc-2.41-x86_64} if check.sh host-prep
# exposed a substitute store, else BUILD it from the 229-byte seed (directive 1; the daily full suite
# is the sole from-seed authoritative builder). The NATIVE gcc itself is ALWAYS BUILT here — it is the
# new artifact, never fetched. The cross rungs + the native rungs both live in tests/x86_64-cross-fns.sh.
#
# Legs (DURABLE — no guix oracle in any):
#   [pinned-input] the chain + gcc-14.3.0 + gmp/mpfr/mpc + binutils-2.44 + glibc-2.41 + x86_64 kernel headers match sha256 (checked by the sourced lib).
#   [native-arch]  the produced gcc/cc1/as/ld ARE ELF 64-bit x86_64 — a native compiler, not the i686 cross gcc.
#   [no-guix]      no /gnu/store in the native gcc/cc1, native as/ld, or the x86_64 libc.so.6.
#   [content-addr] the native gcc is interned at /td/store/<nar-hash>-gcc-14.3.0-x86_64-native.
#   [self-host-compile] the native gcc RUNS in the own-root and compiles a C AND a C++ program from source → both run → 42.
#   [structural]   inside the own-root /td/store IS the store AND /gnu/store is ABSENT.
set -eu

# --- source the x86_64 toolchain gate as a FUNCTION LIBRARY (defines build_* incl. the rung-X2 native
# functions, sets/verifies the pinned-input vars incl. the x86_64 kernel headers, returns BEFORE its
# driver). In scope after: ROOT, fail(), make_curated_path, the cross + native build_* rungs, KH_X86_64_TB.
export TD_X86_64_LIB=1
. tests/bootstrap-x86_64-toolchain-store-native.sh
unset TD_X86_64_LIB
. tests/cache-lib.sh
. tests/x86_64-cross-fns.sh
. tests/x86_64-subst-lib.sh

cpath=`make_curated_path`
for bad in gcc g++ cc guile guix; do test ! -e "$cpath/$bad" || fail "curated PATH still exposes '$bad'"; done
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
export TD_STORE_DIR=/td/store
snwork=`mktemp -d`
trap 'rm -rf "$snwork"' EXIT INT TERM

# the closure store: a FRESH /td/store own-root that holds the fetched/built cross closure components
# (as siblings, so the cross gcc tooldir's relative as/ld symlinks resolve).
cstore="$snwork/closure-store"; cdb="$snwork/closure.db"; mkdir -p "$cstore"

# --- obtain the CROSS toolchain {XBU, XGCC2, XGLIBC}: FETCH the lock-keyed closure, else build from
# seed (x86_64_obtain_cross_toolchain — the former inline block, now shared with the X3 gate).
x86_64_obtain_cross_toolchain "$cpath" "$cstore" "$cdb"

# --- RUNG X2: FETCH the NATIVE x86_64 toolchain from the subst store, else BUILD it from the cross
# toolchain. The native binutils-2.44 + gcc-14.3.0 are input-addressed (tests/td-toolchain-x86_64-native
# .lock, gate 419 / #264); a HIT skips the ~45-min native build. On MISS td builds them from the cross
# toolchain and interns them at their lock-keyed paths + subst-exports them for the daily to sign+publish
# (from-BUILD fallback — directive 1; the daily is the sole authoritative from-cross builder+publisher).
# (x86_64_obtain_native_toolchain — the former inline block, now shared with the X3 gate.)
ncstore="$snwork/native-closure-store"; ncdb="$snwork/native-closure.db"; mkdir -p "$ncstore"
x86_64_obtain_native_toolchain "$cpath" "$ncstore" "$ncdb" "`pwd`/.td-build-cache/x86_64-native-closure-export"

echo ">> [N3] DURABLE own-root verify: the native gcc compiles + runs C/C++ from /td/store → 42"
export XNBU XNGCC XGLIBC
verify_x86_64_native_ownroot "$cpath" "$snwork" || fail "the native x86_64 gcc own-root verify failed"

echo "PASS: x86_64-native-gcc (rung X2) — from the cross toolchain (fetched or from-seed), td built a NATIVE"
echo "      x86_64 binutils 2.44 + GCC 14.3.0 (ELF 64-bit gcc/cc1/g++), interned the native gcc at /td/store,"
echo "      and RAN it in the store-ns own-root where it compiled a C AND a C++ program from source → both 42,"
echo "      /gnu/store ABSENT — a native, self-hosting-arch x86_64 compiler living in td's own store."
