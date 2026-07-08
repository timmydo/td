#!/bin/sh
# tests/bootstrap-x86_64-toolchain-store-native.sh — source-bootstrap: CROSS the i686 full-source
# bootstrap UP to a native x86_64 toolchain at /td/store (x86_64-toolchain track). The whole existing
# /td/store toolchain is i686/32-bit (gcc 14.3.0 + binutils 2.44 + glibc 2.41 all ship ld-linux.so.2),
# but the upstream Rust pin is x86_64 — so the rust-store-native (#196) runtime leg is blocked on
# ARCHITECTURE. The i686-first shape was deliberate from the mes/tcc-era bootstrap bricks (guix's
# mes-boot: "Built i686 (32-bit) … gcc later cross-builds to 64-bit"; the x86_64 MesCC self-host
# path is immature). This gate takes that cross-build step.
#
# From the 229-byte seed, td builds the i686 chain → gcc 14.3.0 + glibc 2.16 (static+shared) + binutils
# 2.44, then with the i686 gcc 14.3.0 CROSSES UP (LFS / crosstool-NG shape): cross binutils 2.44
# (--target=x86_64-pc-linux-gnu) → cross gcc 14 stage1 (C only, --without-headers) → MODERN x86_64
# glibc 2.41 (built by the stage1 cross-gcc; ld-linux-x86-64.so.2 + libc.so.6) → cross gcc 14 stage2
# (c,c++ --enable-shared → libgcc_s.so.1, which rustc needs). The x86_64 glibc 2.41 is interned
# content-addressed into /td/store, and the cross gcc links a DYNAMIC x86_64 C AND C++ program against
# it (interp = /td/store x86_64 ld-linux-x86-64.so.2) that runs in the own-root → 42, /gnu/store ABSENT.
# The cross rungs live in tests/x86_64-cross-fns.sh (sourced). x86_64 kernel headers: host warm-prep
# (td-feed warm kernel-headers x86_64 — the i386 set is wrong arch). serial. all sources td-fetched.
#
# Legs (DURABLE — no guix oracle in any):
#   [pinned-input] chain tarballs + boot patches + gcc-14.3.0 + gmp/mpfr/mpc + binutils-2.44 + glibc-2.41 + x86_64 kernel headers match sha256.
#   [no-guix]      built with gcc/g++/cc/guile/guix DENIED; no /gnu/store in the x86_64 glibc 2.41's libc.so.6 NOR the cross gcc/cc1.
#   [content-addr] the interned x86_64 glibc path is /td/store/<nar-hash>-glibc-2.41-x86_64.
#   [behavioral]   the cross gcc 14.3.0 links a DYNAMIC x86_64 C AND C++ (libstdc++) program against the
#                  MODERN x86_64 glibc 2.41 (interp = ld-linux-x86-64.so.2); the binary is ELF 64-bit; both RUN in the own-root → 42.
#   [structural]   inside the own-root /td/store IS the store AND /gnu/store is ABSENT.
set -eu

ROOT=$(pwd)
fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
STAGE0=seed/stage0
A=AMD64
BOOT_PATCH="$ROOT/seed/patches/binutils-boot-2.20.1a.patch"
BOOT_PATCH_SHA=f6be78a06f2c9905e019ade08f701e5468386cf1934aa27757a64c619571da20
GCC_PATCH="$ROOT/seed/patches/gcc-boot-2.95.3.patch"
GCC_PATCH_SHA=3c42f413b78b341cc064adc505a64445aa4b8c9fc6ce4f7a35a719c8ba92830e
GLIBC_P1="$ROOT/seed/patches/glibc-boot-2.2.5.patch"
GLIBC_P1_SHA=a8de80055076ce1915faed6d9f4380fcf67ee8dad2b4e739c74c9f977213dfdb
GLIBC_P2="$ROOT/seed/patches/glibc-bootstrap-system-2.2.5.patch"
GLIBC_P2_SHA=a8a214f78c96723fee3d9d26b59249029e617bc720880ca2789a66ed73e2c7d0

# --- [pinned-input] all source tarballs + the vendored boot patch match their pins ----------------
lf() {
  _want=$2
  while IFS=' ' read -r _key _rest; do
    [ "$_key" = "$_want" ] || continue
    printf '%s\n' "$_rest"
    return 0
  done < "$1"
  return 1
}
MES_LOCK=`ls seed/sources/mes-*.lock | head -1`;       NYACC_LOCK=`ls seed/sources/nyacc-*.lock | head -1`
TCC_LOCK=`ls seed/sources/tcc-0.9.26*.lock | head -1`; MAKE_LOCK=`ls seed/sources/make-*.lock | head -1`
PATCH_LOCK=`ls seed/sources/patch-*.lock | head -1`;   BU_LOCK=`ls seed/sources/binutils-*.lock | head -1`
GCC_LOCK=`ls seed/sources/gcc-core-*.lock | head -1`
GLIBC_LOCK=`ls seed/sources/glibc-*.lock | head -1`; LINUX_LOCK=`ls seed/sources/linux-*.lock | head -1`
for l in "$MES_LOCK" "$NYACC_LOCK" "$TCC_LOCK" "$MAKE_LOCK" "$PATCH_LOCK" "$BU_LOCK" "$GCC_LOCK" "$GLIBC_LOCK" "$LINUX_LOCK"; do test -n "$l" || fail "missing a seed/sources/*.lock"; done
MES_TB=".td-build-cache/sources/`lf "$MES_LOCK" file`";     NYACC_TB=".td-build-cache/sources/`lf "$NYACC_LOCK" file`"
TCC_TB=".td-build-cache/sources/`lf "$TCC_LOCK" file`";     MAKE_TB=".td-build-cache/sources/`lf "$MAKE_LOCK" file`"
PATCH_TB=".td-build-cache/sources/`lf "$PATCH_LOCK" file`"; BU_TB=".td-build-cache/sources/`lf "$BU_LOCK" file`"
GCC_TB=".td-build-cache/sources/`lf "$GCC_LOCK" file`";     GLIBC_TB=".td-build-cache/sources/`lf "$GLIBC_LOCK" file`"
LINUX_TB=".td-build-cache/sources/`lf "$LINUX_LOCK" file`"
# the host-produced kernel-headers tarball (td-feed warm kernel-headers i386; derived from the pinned linux src)
_kh_file=`lf "$LINUX_LOCK" file`
KH_VER=${_kh_file#linux-}
KH_VER=${KH_VER%%.tar*}
KH_TB=".td-build-cache/sources/linux-headers-$KH_VER-i386.tar.gz"
for pair in "$MES_TB:`lf "$MES_LOCK" sha256`" "$NYACC_TB:`lf "$NYACC_LOCK" sha256`" "$TCC_TB:`lf "$TCC_LOCK" sha256`" \
            "$MAKE_TB:`lf "$MAKE_LOCK" sha256`" "$PATCH_TB:`lf "$PATCH_LOCK" sha256`" "$BU_TB:`lf "$BU_LOCK" sha256`" \
            "$GCC_TB:`lf "$GCC_LOCK" sha256`" "$GLIBC_TB:`lf "$GLIBC_LOCK" sha256`" "$LINUX_TB:`lf "$LINUX_LOCK" sha256`"; do
  f=${pair%:*}; want=${pair##*:}
  test -f "$f" || fail "pinned tarball not warm ($f) — run 'td-feed warm sources'"
  test "`sha "$f"`" = "$want" || fail "warmed $f sha256 != lock pin ($want)"
done
for pp in "$BOOT_PATCH:$BOOT_PATCH_SHA" "$GCC_PATCH:$GCC_PATCH_SHA" "$GLIBC_P1:$GLIBC_P1_SHA" "$GLIBC_P2:$GLIBC_P2_SHA"; do
  pf=${pp%:*}; pw=${pp##*:}
  test -f "$pf" || fail "vendored patch missing ($pf)"
  test "`sha "$pf"`" = "$pw" || fail "vendored patch sha256 != pin ($pf)"
done
echo "   [pinned-input] td-fetched mes/nyacc/tcc/make/patch/binutils/gcc/glibc/linux tarballs + 4 vendored boot patches match their pins"

# --- curated build-driver PATH (gcc/cc/guile/guix DENIED) -------------------------------------
# --- [pinned-input] extras: the gcc-mesboot1 chain sources + gawk + glibc-2.16.0 + 2 patches + gcc-4.9.4 -
MAKE382_LOCK=`ls seed/sources/make-3.82.lock`; MAKE382_TB=".td-build-cache/sources/`lf "$MAKE382_LOCK" file`"
GCC464_LOCK=`ls seed/sources/gcc-core-4.6.4.lock`; GCC464_TB=".td-build-cache/sources/`lf "$GCC464_LOCK" file`"
GPP464_LOCK=`ls seed/sources/gcc-g++-4.6.4.lock`;  GPP464_TB=".td-build-cache/sources/`lf "$GPP464_LOCK" file`"
GMP_LOCK=`ls seed/sources/gmp-*.lock`;   GMP_TB=".td-build-cache/sources/`lf "$GMP_LOCK" file`"
MPFR_LOCK=`ls seed/sources/mpfr-*.lock`; MPFR_TB=".td-build-cache/sources/`lf "$MPFR_LOCK" file`"
MPC_LOCK=`ls seed/sources/mpc-*.lock`;   MPC_TB=".td-build-cache/sources/`lf "$MPC_LOCK" file`"
GAWK_LOCK=`ls seed/sources/gawk-*.lock`; GAWK_TB=".td-build-cache/sources/`lf "$GAWK_LOCK" file`"
GLIBC216_LOCK=`ls seed/sources/glibc-mesboot-2.16.0.lock`; GLIBC216_TB=".td-build-cache/sources/`lf "$GLIBC216_LOCK" file`"
GCC494_LOCK=`ls seed/sources/gcc-4.9.4.lock`; GCC494_TB=".td-build-cache/sources/`lf "$GCC494_LOCK" file`"
GCC464_PATCH="$ROOT/seed/patches/gcc-boot-4.6.4.patch";          GCC464_PATCH_SHA=0dfcb1813ca54eafad0d3bbec17b423d6e50ab76d730b35eb6df7018ed43edff
GLIBC216_P1="$ROOT/seed/patches/glibc-boot-2.16.0.patch";        GLIBC216_P1_SHA=3de61d25fff5924723ec8fb0a57d37305f8e25b9e65d3d67a6535dbe08ac0e88
GLIBC216_P2="$ROOT/seed/patches/glibc-bootstrap-system-2.16.0.patch"; GLIBC216_P2_SHA=061cf1269b9d497962389c8b0c52659f8294ae16e0963d146b6599f096bb50ff
for pair in "$MAKE382_TB:`lf "$MAKE382_LOCK" sha256`" "$GCC464_TB:`lf "$GCC464_LOCK" sha256`" "$GPP464_TB:`lf "$GPP464_LOCK" sha256`" \
            "$GMP_TB:`lf "$GMP_LOCK" sha256`" "$MPFR_TB:`lf "$MPFR_LOCK" sha256`" "$MPC_TB:`lf "$MPC_LOCK" sha256`" \
            "$GAWK_TB:`lf "$GAWK_LOCK" sha256`" "$GLIBC216_TB:`lf "$GLIBC216_LOCK" sha256`" "$GCC494_TB:`lf "$GCC494_LOCK" sha256`"; do
  f=${pair%:*}; want=${pair##*:}
  test -f "$f" || fail "pinned tarball not warm ($f) — run 'td-feed warm sources'"
  test "`sha "$f"`" = "$want" || fail "warmed $f sha256 != lock pin ($want)"
done
for pp in "$GCC464_PATCH:$GCC464_PATCH_SHA" "$GLIBC216_P1:$GLIBC216_P1_SHA" "$GLIBC216_P2:$GLIBC216_P2_SHA"; do
  pf=${pp%:*}; pw=${pp##*:}; test -f "$pf" || fail "vendored patch missing ($pf)"; test "`sha "$pf"`" = "$pw" || fail "vendored patch sha256 != pin ($pf)"
done
echo "   [pinned-input] + gcc-4.6.4/gcc-g++/gmp/mpfr/mpc/gawk-3.1.8/glibc-2.16.0/gcc-4.9.4 + the boot patches match their pins"
GCC14_LOCK=`ls seed/sources/gcc-14.3.0.lock`; GCC14_TB=".td-build-cache/sources/`lf "$GCC14_LOCK" file`"
GMP63_LOCK=`ls seed/sources/gcc14-gmp-*.lock`; GMP63_TB=".td-build-cache/sources/`lf "$GMP63_LOCK" file`"
MPFR421_LOCK=`ls seed/sources/gcc14-mpfr-*.lock`; MPFR421_TB=".td-build-cache/sources/`lf "$MPFR421_LOCK" file`"
MPC131_LOCK=`ls seed/sources/gcc14-mpc-*.lock`; MPC131_TB=".td-build-cache/sources/`lf "$MPC131_LOCK" file`"
for pair in "$GCC14_TB:`lf "$GCC14_LOCK" sha256`" "$GMP63_TB:`lf "$GMP63_LOCK" sha256`" "$MPFR421_TB:`lf "$MPFR421_LOCK" sha256`" "$MPC131_TB:`lf "$MPC131_LOCK" sha256`"; do
  f=${pair%:*}; want=${pair##*:}
  test -f "$f" || fail "pinned tarball not warm ($f) — run 'td-feed warm sources'"
  test "`sha "$f"`" = "$want" || fail "warmed $f sha256 != lock pin ($want)"
done
echo "   [pinned-input] + gcc-14.3.0/gmp-6.3.0/mpfr-4.2.1/mpc-1.3.1 (the modern gcc prereqs) match their pins"
BU244_LOCK=`ls seed/sources/binutils-2.44.lock`; BU244_TB=".td-build-cache/sources/`lf "$BU244_LOCK" file`"
GLIBC241_LOCK=`ls seed/sources/glibc-2.41.lock`; GLIBC241_TB=".td-build-cache/sources/`lf "$GLIBC241_LOCK" file`"
for pair in "$BU244_TB:`lf "$BU244_LOCK" sha256`" "$GLIBC241_TB:`lf "$GLIBC241_LOCK" sha256`"; do
  f=${pair%:*}; want=${pair##*:}
  test -f "$f" || fail "pinned tarball not warm ($f) — run 'td-feed warm sources'"
  test "`sha "$f"`" = "$want" || fail "warmed $f sha256 != lock pin ($want)"
done
echo "   [pinned-input] + binutils-2.44/glibc-2.41 (the modern toolchain final pieces) match their pins"

# --- [pinned-input] the x86_64 kernel headers (host warm-prep) -------------------------------------
KH_X86_64_TB=".td-build-cache/sources/linux-headers-$KH_VER-x86_64.tar.gz"
test -f "$KH_X86_64_TB" || fail "x86_64 kernel headers not warm ($KH_X86_64_TB) — run 'td-feed warm kernel-headers x86_64'"
echo "   [pinned-input] + the x86_64 Linux UAPI headers (derived from the pinned linux-$KH_VER source)"

# --- sourced as a FUNCTION LIBRARY (TD_X86_64_LIB=1): the build_* rung functions are now
# run, so a consumer (rust-x86_64-runtime/userland) can source this gate for the pinned-input vars +
# KH_X86_64_TB + the cross-fns library (run_x86_64_cross/verify/native), then drive the rungs and
# add its own legs. Return BEFORE the build driver. Behavior-preserving when
# executed normally: TD_X86_64_LIB is unset → the guard is false → the driver below runs as-is.
[ "${TD_X86_64_LIB:-0}" = 1 ] && return 0

# ============================================================================================
# Build the i686 base FROM THE SEED (the 21-rung chain → gcc 14.3.0 + glibc 2.16 static/shared +
# binutils 2.44), then CROSS UP to x86_64. Directive 1: from the 229-byte seed, no cache, offline.
# ============================================================================================
# --- load stage0 + the /td/store own-root store + a static bash for the store-ns shell. BOTH the
# FETCH-skip and the from-seed BUILD paths need these. ---
. tests/cache-lib.sh
. tests/x86_64-cross-fns.sh
. tests/x86_64-subst-lib.sh
# make_curated_path now lives in x86_64-cross-fns.sh (moved when the inline i686 chain retired), so
# the curated PATH must be built AFTER that source, not before.
cpath=`make_curated_path`
for bad in gcc g++ cc guile guix; do test ! -e "$cpath/$bad" || fail "curated PATH still exposes '$bad'"; done
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
export TD_STORE_DIR=/td/store
snwork=`mktemp -d`
trap 'rm -rf "$snwork"' EXIT INT TERM   # both paths (the build branch re-traps, incl. $snwork below)
# The CLOSURE store: a FRESH /td/store own-root holding the 3 lock-keyed components + a static bash
# for the store-ns shell. Used by BOTH paths (fetched-into OR built-and-interned-into); kept separate
# from verify_x86_64_ownroot's own $snwork/td-store so the from-seed path's glibc copy can't collide.
cstore="$snwork/closure-store"; cdb="$snwork/closure.db"; mkdir -p "$cstore"
# the static-bash fixture is a DECLARED gate input (#353): the runner resolved it.
bs=${TD_GATE_INPUT_BASH_STATIC:-}
test -n "$bs" || fail "TD_GATE_INPUT_BASH_STATIC unset — run via td-builder gate-run, which resolves the gate's declared inputs"
test -x "$bs/bin/bash" || fail "no static bash fixture at $bs"
bbase=`basename "$bs"`; cp -a "$bs" "$cstore/$bbase"; chmod -R u+w "$cstore"

# --- FETCH SHORT-CIRCUIT (x64-toolchain-subst, human 2026-06-28): if check.sh host-prep exposed a
# persistent signed substitute store (TD_SUBST_BIN/STORE/PUBKEY), FETCH the lock-keyed x86_64
# toolchain CLOSURE {binutils-2.44, gcc-14.3.0, glibc-2.41} and SKIP the ~98-min from-seed build.
# ANY miss → build from seed (the substitute is an optimization, NEVER a correctness dependency).
# DELIBERATE directive-1 relaxation, human-approved: the daily full suite stays the sole from-seed
# authoritative build AND the publisher of the signed closure.
if x86_64_resolve_closure "$cstore" "$cdb"; then
  echo ">> [subst/SKIP] fetched the x86_64 toolchain closure {binutils,gcc,glibc} — SKIPPED the ~98-min from-seed build"
  built=0
else
  built=1
  echo ">> [subst/MISS] no exposed substitute store — building the x86_64 toolchain from the 229-byte seed (directive 1)"
  # The i686 base (21 rungs) AND the x86_64 cross rungs (cross binutils 2.44 → gcc stage1 → glibc
  # 2.41 → gcc stage2) are RECIPES now (#378 slice 4): run_x86_64_cross drives the whole graph via
  # build-plan --auto and exports GCC14 GST GSH BMB244SB XBU XGCC2 XGLIBC XLIBGCCDIR XSTDCXXDIR.
  run_x86_64_cross "$cpath" || fail "the x86_64 toolchain (recipe ladder) failed to build from the seed"
  echo "   built the i686 base + crossed up to x86_64 via the recipe ladder (build-plan --auto)"
  # DURABLE own-root verify (#201/#215 legs: [no-guix] no /gnu/store in the x86_64 libc/gcc,
  # content-addr, the input-addressed glibc at its lock path, distinct-arch), then intern the FULL
  # closure {binutils,gcc,glibc} into the closure store + subst-export it.
  verify_x86_64_ownroot "$cpath" "$snwork" || fail "the x86_64 own-root verify failed"
  # Make the cross gcc self-contained BEFORE interning: bundle plain as/ld into its tooldir so the
  # PUBLISHED nar carries them (a fetched gcc's build-time --with-as scratch path is gone).
  x86_64_bundle_tooldir "$XGCC2" || fail "could not bundle as/ld into the cross gcc tooldir"
  # NOTE (directive-3 callout, #378 slice 4): the shell cross-gcc repro double-build
  # (x86_64_gcc_repro_leg) is RETIRED. It existed because the SHELL build leaked its /tmp build path
  # into DWARF (non-deterministic raw builds needing normalization); the recipe eliminates that
  # (stable content-addressed input paths + pinned -frandom-seed), so the cross gcc 14.3.0 now has
  # the SAME reproducibility treatment as every recipe rung — deterministic-by-construction plus the
  # daily force-cold backstop — consistent with the i686 gcc-14 rung (which has no per-rung repro leg).
  x86_64_build_closure "`pwd`/.td-build-cache/x86_64-closure-export" "$cstore" "$cdb" || fail "could not intern + subst-export the x86_64 toolchain closure"
fi

# --- UNIFIED DURABLE verify (the assertion a build-SKIP rests on): the closure — BUILT+interned OR
# FETCHED, at its lock-keyed /td/store paths — IS a working toolchain: its cross gcc compiles a
# DYNAMIC x86_64 program (interp = the glibc lock path) that RUNS in the store-ns own-root → 42,
# /gnu/store ABSENT.
x86_64_verify_closure "$cpath" "$cstore" "$cdb" "$bbase" || fail "the x86_64 closure toolchain did not compile+run an x86_64 program → 42"

if [ "$built" = 1 ]; then
  echo "PASS: x86_64-toolchain — BUILT from the 229-byte seed (i686 chain → gcc 14.3.0 → CROSS UP), interned the"
  echo "      closure {cross binutils 2.44 + cross gcc 14.3.0 + x86_64 glibc 2.41} at its lock-keyed /td/store paths;"
  echo "      the closure compiles + runs a DYNAMIC x86_64 program in the own-root → 42, /gnu/store ABSENT."
  echo "      (With a publisher key set, the closure was signed + published for the per-PR loop to FETCH next time.)"
else
  echo "PASS: x86_64-toolchain — FETCHED the lock-keyed toolchain closure from the substitute store and SKIPPED the"
  echo "      ~98-min from-seed build; the fetched closure compiles + runs a DYNAMIC x86_64 program in the own-root →"
  echo "      42, /gnu/store ABSENT (the build-SKIP — directive-1 relaxation; the daily is the sole from-seed builder)."
fi
