#!/bin/sh
# tests/bootstrap-x86_64-self-gcc-store-native.sh — source-bootstrap: rung X3 of the x86_64-toolchain
# track — SELF-HOSTING (gcc rebuilds gcc). Rung X2 (gate 422) produced a NATIVE x86_64 toolchain, but
# its BUILDER was the i686 CROSS gcc (an ELF 32-bit binary): the toolchain became native, the
# bootstrap step that produced it was not. X3 closes the loop: with the NATIVE /td/store toolchain
# (fetched at its lock-keyed paths, or built from the cross toolchain, itself fetched or built from
# the 229-byte seed) td REBUILDS x86_64 binutils 2.44 + GCC 14.3.0 — the compiler that compiles the
# compiler is itself an x86_64 binary living in td's own store, the from-source gcc-rebuilds-gcc
# milestone gate 422 explicitly did not claim. The SELF toolchain is ALWAYS BUILT here (never
# fetched) — it is the new artifact; only its native-toolchain prerequisite may be fetched.
#
# Legs (DURABLE — no guix oracle in any):
#   [pinned-input]  chain + gcc-14.3.0 + gmp/mpfr/mpc + binutils-2.44 + glibc-2.41 + x86_64 kernel headers match sha256 (checked by the sourced lib).
#   [builder-arch]  IN-RECIPE (toolchain-recipe x86_64-self): the gcc DRIVING the rebuild is itself
#                   ELF64 x86_64 — an i686 builder (rung X2's cross gcc) reds; X2 cannot stand in.
#   [codegen]       the INPUT native gcc and the SELF-rebuilt gcc emit BYTE-IDENTICAL -O2 -S assembly
#                   for a fixed C AND C++ TU (GCC's stage2-vs-stage3 fixpoint, at the text level).
#   [native-arch]   the SELF-rebuilt gcc/binutils ARE ELF 64-bit x86_64 binaries.
#   [no-guix]       no /gnu/store bytes in the self gcc/cc1, self as/ld, or the x86_64 libc.so.6.
#   [content-addr]  the self gcc is interned at /td/store/<nar-hash>-gcc-14.3.0-x86_64-self (the
#                   verify's name assert keeps it distinct from the X2 native artifact).
#   [self-host-compile] the SELF-rebuilt gcc RUNS in the store-ns own-root and compiles a C AND a C++
#                   program from source → both run → 42.
#   [structural]    inside the own-root /td/store IS the store AND /gnu/store is ABSENT.
set -eu

# --- source the x86_64 toolchain gate as a FUNCTION LIBRARY (defines the chain build_* fns, sets/
# verifies the pinned-input vars incl. the x86_64 kernel headers, returns BEFORE its driver). In
# scope after: ROOT, fail(), make_curated_path, the cross + native + self rungs, KH_X86_64_TB.
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
trap 'rm -rf "$snwork" "${srout:-}"' EXIT INT TERM

# the closure store: a FRESH /td/store own-root that holds the fetched/built cross closure components
# (as siblings, so the cross gcc tooldir's relative as/ld symlinks resolve).
cstore="$snwork/closure-store"; cdb="$snwork/closure.db"; mkdir -p "$cstore"

# --- obtain the CROSS toolchain {XBU, XGCC2, XGLIBC}: FETCH the lock-keyed closure, else build from
# seed. Needed for XGLIBC (the self build links vs it) and as the native build's input on a MISS.
x86_64_obtain_cross_toolchain "$cpath" "$cstore" "$cdb"

# --- obtain the NATIVE toolchain {XNBU, XNGCC} (rung X2's artifact): FETCH it at its lock-keyed
# paths, else BUILD it from the cross toolchain (+ intern/subst-export for the daily to publish).
ncstore="$snwork/native-closure-store"; ncdb="$snwork/native-closure.db"; mkdir -p "$ncstore"
x86_64_obtain_native_toolchain "$cpath" "$ncstore" "$ncdb" "`pwd`/.td-build-cache/x86_64-native-closure-export"

# --- RUNG X3: ALWAYS build the SELF toolchain — the native toolchain rebuilds binutils 2.44 + gcc
# 14.3.0. The recipe's [builder-arch] leg asserts the driving gcc is ELF64 x86_64 (self-hosting).
echo ">> [X3] SELF-HOSTED rebuild: the NATIVE /td/store toolchain rebuilds binutils 2.44 + gcc 14.3.0 (toolchain-recipe x86_64-self)"
srout=`mktemp -d`/self-out
x86_64_build_self_recipe "$cpath" "$XNGCC" "$XNBU" "$XGLIBC" "$srout" || fail "the native toolchain could not rebuild itself (toolchain-recipe x86_64-self)"

echo ">> [codegen] fixpoint: the input native gcc and the SELF-rebuilt gcc emit identical -O2 -S assembly (C + C++)"
x86_64_self_codegen_agreement "$XNGCC" "$XSGCC" || fail "codegen agreement between the native gcc and the self-rebuilt gcc failed"

echo ">> [verify] DURABLE own-root verify: the SELF-rebuilt gcc compiles + runs C/C++ from /td/store → 42"
# point the shared verify at the SELF trees; the positional name arg pins the -self artifact name.
XNGCC="$XSGCC"; XNBU="$XSBU"
export XNBU XNGCC XGLIBC
verify_x86_64_native_ownroot "$cpath" "$snwork" gcc-14.3.0-x86_64-self || fail "the self-rebuilt x86_64 gcc own-root verify failed"

echo "PASS: x86_64-self-gcc (rung X3) — with the NATIVE /td/store toolchain (fetched or from-cross/seed)"
echo "      as the BUILDER, td rebuilt x86_64 binutils 2.44 + GCC 14.3.0: gcc-rebuilds-gcc. The rebuilt"
echo "      compiler emits byte-identical -O2 -S assembly to its builder (stage2/stage3 fixpoint), was"
echo "      interned at /td/store as gcc-14.3.0-x86_64-self, and RAN in the store-ns own-root where it"
echo "      compiled a C AND a C++ program from source → both 42, /gnu/store ABSENT — a self-hosting"
echo "      x86_64 toolchain living entirely in td's own store."
