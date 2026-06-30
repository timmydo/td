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

# --- obtain the CROSS toolchain {XBU, XGCC2, XGLIBC}: FETCH the lock-keyed closure, else build from seed
if x86_64_resolve_closure "$cstore" "$cdb"; then
  echo ">> [subst/SKIP] fetched the x86_64 cross toolchain closure {binutils,gcc,glibc} — SKIPPED the ~98-min from-seed build"
else
  echo ">> [subst/MISS] no exposed substitute store — building the cross toolchain from the 229-byte seed (directive 1)"
  tc=`build_toolchain` || fail "the seed toolchain (brick 0+1) did not build"
  mesp=`build_mes_prefix "$tc" "$cpath"` || fail "Mes (MesCC self-host) did not build/install"
  TCCD=`mktemp -d`/tcc; build_tcc "$tc" "$cpath" "$mesp" "$TCCD" || fail "MesCC did not build tcc"
  MK=`mktemp -d`/makebuild; build_make "$tc" "$cpath" "$mesp" "$TCCD" "$MK" || fail "tcc did not build GNU Make 3.80"
  PD=`mktemp -d`/patchbuild; build_patch "$cpath" "$mesp" "$TCCD" "$MK" "$PD" || fail "the tcc-built make did not build patch"
  BD=`mktemp -d`/binutilsbuild; build_binutils "$cpath" "$mesp" "$TCCD" "$MK" "$PD" "$BD" || fail "the tcc-built make did not build binutils-mesboot0"
  GD=`mktemp -d`/gccbuild; build_gcc "$cpath" "$mesp" "$TCCD" "$MK" "$PD" "$BD" "$GD" || fail "the toolchain did not build gcc 2.95.3"
  HD=`mktemp -d`/headers; build_headers "$mesp" "$HD" || fail "could not install the kernel headers"
  GLD=`mktemp -d`/glibcbuild; build_glibc "$cpath" "$GD" "$BD" "$TCCD" "$MK" "$PD" "$HD" "$GLD" || fail "the seed toolchain did not build glibc 2.2.5"
  G2=`mktemp -d`/gcc2build; build_gcc_mesboot0 "$cpath" "$GD" "$BD" "$GLD" "$HD" "$MK" "$PD" "$G2" || fail "the toolchain did not rebuild gcc 2.95.3 against glibc"
  B2=`mktemp -d`/binutils1build; build_binutils_mesboot1 "$cpath" "$G2" "$BD" "$GLD" "$MK" "$PD" "$B2" || fail "gcc-mesboot0 did not rebuild binutils against glibc"
  MM=`mktemp -d`/makemesbootbuild; build_make_mesboot "$cpath" "$G2" "$BD" "$GLD" "$MK" "$MM" || fail "gcc-mesboot0 did not rebuild GNU Make against glibc"
  GM1=`mktemp -d`/gccmesboot1build; build_gcc_mesboot1 "$cpath" "$G2" "$B2" "$MM" "$GLD" "$PD" "$GM1" || fail "the toolchain did not build GCC 4.6.4 (c,c++)"
  BMB=`mktemp -d`/binutilsmesbootbuild; build_binutils_mesboot "$cpath" "$GM1" "$B2" "$GLD" "$MM" "$PD" "$BMB" || fail "gcc-mesboot1 did not rebuild binutils"
  GAWKMB=`mktemp -d`/gawkmesbootbuild; build_gawk_mesboot "$cpath" "$GM1" "$B2" "$GLD" "$MM" "$GAWKMB" || fail "gcc-mesboot1 did not build GNU awk"
  GOUT=`mktemp -d`/glibcmesbootbuild; build_glibc_mesboot "$cpath" "$GM1" "$BMB" "$GAWKMB" "$GLD" "$MM" "$PD" "$GOUT" || fail "the toolchain did not build glibc 2.16.0"
  GMB=`mktemp -d`/gccmesbootbuild; build_gcc_mesboot "$cpath" "$GM1" "$BMB" "$GOUT" "$MM" "$PD" "$GMB" || fail "the toolchain did not build gcc-mesboot (GCC 4.9.4)"
  GSH=`mktemp -d`/glibcsharedbuild; build_glibc_mesboot_shared "$cpath" "$GM1" "$BMB" "$GAWKMB" "$GLD" "$MM" "$PD" "$GSH" || fail "the toolchain did not build the SHARED glibc 2.16.0"
  GCC14B=`mktemp -d`/gcc14build; build_gcc_14 "$cpath" "$GMB/out" "$GOUT/out" "$BMB/out" "$GCC14B" || fail "the toolchain did not build MODERN GCC 14.3.0"
  BMB244SB=`mktemp -d`/bu244sbbuild; build_binutils_244 "$cpath" "$GM1/out" "$GSH/out" "$BMB/out" "$BMB244SB" || fail "the toolchain did not build the modern binutils 2.44"
  GCC14="$GCC14B/stage/td/store/gcc-14.3.0"; GST="$GOUT/out"
  echo "   built the i686 base: gcc 14.3.0 + glibc 2.16 (static+shared) + binutils 2.44"
  run_x86_64_cross "$cpath" "$GCC14" "$GST" "$GSH/out" "$BMB244SB" "$KH_X86_64_TB" || fail "the x86_64 cross rungs failed"
  # run_x86_64_cross exports XGLIBC XGCC2 XLIBGCCDIR XSTDCXXDIR XBU X86_WORK (physical trees)
fi
test -n "${XGCC2:-}" -a -n "${XGLIBC:-}" -a -n "${XBU:-}" || fail "cross toolchain vars unset after fetch/build"

# --- RUNG X2: build the NATIVE x86_64 toolchain on top of the cross toolchain ---
echo ">> [N1] NATIVE x86_64 binutils 2.44 (ELF 64-bit as/ld)"
XNBU=`mktemp -d`/native-binutils
build_binutils_x86_64_native "$cpath" "$XGCC2" "$XGLIBC" "$XBU" "$KH_X86_64_TB" "$XNBU" || fail "could not build the NATIVE x86_64 binutils"
echo ">> [N2] NATIVE x86_64 gcc 14.3.0 (ELF 64-bit gcc/cc1/g++)"
XNGCCB=`mktemp -d`/native-gcc
build_gcc_x86_64_native "$cpath" "$XGCC2" "$XGLIBC" "$XBU" "$XNBU" "$KH_X86_64_TB" "$XNGCCB" || fail "could not build the NATIVE x86_64 gcc"
XNGCC="$XNGCCB/stage/td/store/gcc-14.3.0-x86_64-native"

echo ">> [N3] DURABLE own-root verify: the native gcc compiles + runs C/C++ from /td/store → 42"
export XNBU XNGCC XGLIBC
verify_x86_64_native_ownroot "$cpath" "$snwork" || fail "the native x86_64 gcc own-root verify failed"

echo "PASS: x86_64-native-gcc (rung X2) — from the cross toolchain (fetched or from-seed), td built a NATIVE"
echo "      x86_64 binutils 2.44 + GCC 14.3.0 (ELF 64-bit gcc/cc1/g++), interned the native gcc at /td/store,"
echo "      and RAN it in the store-ns own-root where it compiled a C AND a C++ program from source → both 42,"
echo "      /gnu/store ABSENT — a native, self-hosting-arch x86_64 compiler living in td's own store."
