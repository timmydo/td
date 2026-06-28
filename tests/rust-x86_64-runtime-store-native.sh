#!/bin/sh
# tests/rust-x86_64-runtime-store-native.sh — rust-store-native track: the /td/store-RUNTIME leg.
# RUN the upstream x86_64 Rust toolchain (rustc + cargo) from /td/store in a store-ns own-root with
# /gnu/store ABSENT — the leg #196 left [PENDING glibc-final]. Both blockers are now resolved: glibc
# 2.41 (#199) AND, decisively, the x86_64 toolchain (#201) — an x86_64 rustc cannot run against an
# i686 glibc, so the leg was blocked on ARCHITECTURE, not just glibc>=2.17.
#
# From the 229-byte seed, td builds the i686 chain -> gcc 14.3.0, then CROSSES UP to a native x86_64
# toolchain (cross binutils 2.44 + cross gcc 14.3.0 + MODERN x86_64 glibc 2.41 + libgcc_s.so.1).
# This gate REUSES that chain WITHOUT copying it: it sources the x86_64 gate as a FUNCTION LIBRARY
# (TD_X86_64_LIB=1 -> the build_* rungs + verified pinned-input vars, NO build driver) and drives the
# rungs itself, then adds the rust-specific runtime leg:
#
#   - the upstream Rust 1.96.0 release tarball (static.rust-lang.org, sha256-pinned, GUIX-FREE) is
#     RELINKED to /td/store by td's OWN ELF rewriter (elf-set-interp -> /td/store/ld, no patchelf);
#   - rustc's full external runtime closure is co-located in the relinked tree's lib/ (found via the
#     UNCHANGED RUNPATH $ORIGIN/../lib): rust's own librustc_driver + libLLVM, the /td/store x86_64
#     glibc 2.41 libs, libgcc_s.so.1, and an x86_64 libz.so.1 BUILT FROM SOURCE here (zlib 1.3.1 by
#     the cross gcc — upstream libLLVM dynamically NEEDs libz, which the toolchain does not provide);
#   - the tree is interned content-addressed at /td/store, and rustc -vV + cargo --version RUN in the
#     store-ns own-root (interp = /td/store/ld) -> "rustc 1.96.0" / "cargo 1.96.0", /gnu/store ABSENT.
#
# Every external runtime dep is td-built-from-seed (glibc/libgcc/libz) or upstream-not-guix (rust),
# so the whole running /td/store rust package carries ZERO /gnu/store bytes. HEAVY (~90 min from the
# seed; directive 1 — no cache for the authoritative gate). NOT a BUILD_GATE.
#
# Legs (DURABLE — no guix oracle in any):
#   [supply-chain]  the rust + zlib tarballs match their lock sha256 (the sha IS the oracle).
#   [provenance]    the upstream rustc/cargo/.so carry zero /gnu/store (upstream-not-guix).
#   [no-guix]       the interned /td/store rust package (rust bins + td-built glibc/libgcc/libz) has
#                   zero /gnu/store anywhere; the relinked interp is /td/store/ld.
#   [structural]    the tree's lib/ closure is COMPLETE (every NEEDED soname present); interp /td/store.
#   [behavioral]    rustc -vV AND cargo --version RUN from /td/store in the store-ns own-root -> the
#                   real version strings. THE durable payoff (an x86_64 rust toolchain that runs with
#                   no guix process and no guix bytes in its store).
#   [structural]    inside the own-root /td/store IS the store AND /gnu/store is ABSENT.
# Self-discrimination (verified-red): dropping libz.so.1 (or libgcc_s.so.1) from the tree's lib/, or
# skipping the elf-set-interp relink, makes the own-root run FAIL — each is load-bearing.
set -eu

# --- source the x86_64 toolchain gate as a FUNCTION LIBRARY (defines build_*, sets/verifies the
# pinned-input vars incl. the x86_64 kernel headers, then returns BEFORE its build driver) ----------
export TD_X86_64_LIB=1
. tests/bootstrap-x86_64-toolchain-store-native.sh
unset TD_X86_64_LIB
# now in scope: ROOT, fail(), sha(), lf(), make_curated_path, the 21 build_* rungs, KH_X86_64_TB.

# --- [supply-chain] the upstream Rust + zlib tarballs match their lock sha256 (upstream, not guix) -
RUST_LOCK=`ls seed/sources/rust-*.lock 2>/dev/null | head -1`
test -n "$RUST_LOCK" || fail "no seed/sources/rust-*.lock pin"
RUST_FILE=`lf "$RUST_LOCK" file`; RUST_TB=".td-build-cache/sources/$RUST_FILE"
test -f "$RUST_TB" || fail "warmed $RUST_TB absent — run tools/warm-bootstrap-sources.sh (host PREP)"
test "`sha "$RUST_TB"`" = "`lf "$RUST_LOCK" sha256`" || fail "warmed $RUST_TB sha256 != lock pin"
ZLIB_LOCK=`ls seed/sources/zlib-*.lock 2>/dev/null | head -1`
test -n "$ZLIB_LOCK" || fail "no seed/sources/zlib-*.lock pin"
ZLIB_TB=".td-build-cache/sources/`lf "$ZLIB_LOCK" file`"
test -f "$ZLIB_TB" || fail "warmed $ZLIB_TB absent — run tools/warm-bootstrap-sources.sh (host PREP)"
test "`sha "$ZLIB_TB"`" = "`lf "$ZLIB_LOCK" sha256`" || fail "warmed $ZLIB_TB sha256 != lock pin"
echo "   [supply-chain] rust ($RUST_FILE) + zlib match their lock sha256 — upstream bytes, not guix"

# build_zlib_x86_64 <cpath> <xgcc2> <xglibc> <xlibgccdir> <xbu> <out> — x86_64 zlib 1.3.1 (libz.so.1)
# built FROM SOURCE by the cross gcc 14.3.0 vs the /td/store x86_64 glibc 2.41. Output: $out/libz.so.1.3.1.
build_zlib_x86_64() {
  zc=$1; xg=$2; xgl=$3; xlg=$4; xb=$5; out=$6
  rm -rf "$out"; mkdir -p "$out"
  csh=`command -v bash 2>/dev/null || command -v sh`
  src=`mktemp -d`/zlib; mkdir -p "$src"
  tar -xzf "$ZLIB_TB" -C "$src" --strip-components=1 || { echo "zlib unpack failed" >&2; return 1; }
  wb=`mktemp -d`/wb; mkdir -p "$wb"
  printf '#!%s\nexec "%s/bin/%s-gcc" -isystem "%s/include" -B"%s/lib" -L"%s/lib" -L"%s" "$@"\n' \
    "$csh" "$xg" "$XTARGET" "$xgl" "$xgl" "$xgl" "$xlg" > "$wb/cc"
  chmod 0555 "$wb/cc"
  ( cd "$src"; bp="$xb/bin:$zc"
    env PATH="$bp" CC="$wb/cc" CHOST="$XTARGET" AR="$xb/bin/$XTARGET-ar" RANLIB="$xb/bin/$XTARGET-ranlib" \
      "$csh" ./configure --prefix=/td/store/zlib-1.3.1 --shared >cfg.log 2>&1 \
      || { echo "zlib configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_zlibx-cfg.log" 2>/dev/null||true; tail -20 cfg.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= make SHELL="$csh" CONFIG_SHELL="$csh" libz.so.1.3.1 >build.log 2>&1 \
      || { echo "zlib make failed" >&2; cp build.log "$ROOT/.td-build-cache/_zlibx-build.log" 2>/dev/null||true; tail -25 build.log >&2; return 1; }
    cp -a libz.so.1.3.1 "$out/libz.so.1.3.1" ) || return 1
  test -f "$out/libz.so.1.3.1" || { echo "no x86_64 libz.so produced" >&2; return 1; }
}

# ============================================================================================
# Build the i686 base FROM THE SEED, then CROSS UP to x86_64 — REUSING the x86_64 gate's rungs
# (sourced above). Directive 1: from the 229-byte seed, no cache, offline.
# ============================================================================================
cpath=`make_curated_path`
for bad in gcc g++ cc guile guix; do test ! -e "$cpath/$bad" || fail "curated PATH still exposes '$bad'"; done
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
ZLIBX=`mktemp -d`/zlibx
trap 'rm -rf "$tc" "$mesp" "`dirname "$TCCD"`" "`dirname "$MK"`" "`dirname "$PD"`" "`dirname "$BD"`" "`dirname "$GD"`" "`dirname "$HD"`" "`dirname "$GLD"`" "`dirname "$G2"`" "`dirname "$B2"`" "`dirname "$MM"`" "`dirname "$GM1"`" "`dirname "$BMB"`" "`dirname "$GAWKMB"`" "`dirname "$GOUT"`" "`dirname "$GMB"`" "`dirname "$GSH"`" "`dirname "$GCC14B"`" "`dirname "$BMB244SB"`" "`dirname "$ZLIBX"`" "`dirname "$cpath"`" "${snwork:-}" "${rtree:-}"' EXIT INT TERM

GCC14="$GCC14B/stage/td/store/gcc-14.3.0"; GST="$GOUT/out"
echo "   built the i686 base: gcc 14.3.0 + glibc 2.16 (static+shared) + binutils 2.44"

# ---- CROSS UP to x86_64 (reused rungs) ----
. tests/x86_64-cross-fns.sh
run_x86_64_cross "$cpath" "$GCC14" "$GST" "$GSH/out" "$BMB244SB" "$KH_X86_64_TB" || fail "the x86_64 cross rungs failed"
# exports: XGLIBC XGCC2 XLIBGCCDIR XSTDCXXDIR XBU X86_WORK
echo "   crossed up: x86_64 glibc 2.41 ($XGLIBC) + libgcc_s ($XLIBGCCDIR)"

# ---- x86_64 zlib (libLLVM needs libz; the toolchain doesn't provide it) ----
build_zlib_x86_64 "$cpath" "$XGCC2" "$XGLIBC" "$XLIBGCCDIR" "$XBU" "$ZLIBX" || fail "the cross gcc did not build x86_64 zlib"
XLIBZ="$ZLIBX/libz.so.1.3.1"
if grep -q -a '/gnu/store' "$XLIBZ"; then fail "the built x86_64 libz contains /gnu/store bytes"; fi
echo "   built x86_64 libz.so.1 from zlib 1.3.1 source (cross gcc 14.3.0, no /gnu/store)"

# --- td's guix-free stage0 builder (for the ELF relink + store intern + own-root run) -------------
. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
export TD_STORE_DIR=/td/store

# --- extract the upstream x86_64 rustc + cargo + rustc's bundled .so (skip the bulky rustlib) -----
rtree=`mktemp -d`/r; mkdir -p "$rtree/x"
top="${RUST_FILE%.tar.gz}"
tar -xzf "$RUST_TB" -C "$rtree/x" --exclude="$top/rustc/lib/rustlib" \
  "$top/rustc/bin/rustc" "$top/rustc/lib" "$top/cargo/bin/cargo" || fail "rust tarball extract failed"
rx="$rtree/x/$top"
tree="$rtree/tree"; mkdir -p "$tree/bin" "$tree/lib"
cp "$rx/rustc/bin/rustc" "$tree/bin/rustc"
cp "$rx/cargo/bin/cargo" "$tree/bin/cargo"
cp -a "$rx"/rustc/lib/*.so* "$tree/lib/" 2>/dev/null || true

# --- [provenance] the upstream binaries + .so carry NO /gnu/store ----------------------------------
for b in "$tree/bin/rustc" "$tree/bin/cargo" "$tree"/lib/librustc_driver-*.so; do
  test -e "$b" || continue
  ! grep -q -a '/gnu/store' "$b" || fail "$b contains /gnu/store bytes — not guix-free upstream"
done
echo "   [provenance] upstream rustc + cargo + librustc_driver carry zero /gnu/store bytes"

# --- co-locate the FULL external runtime closure in the tree's lib/ (found via RUNPATH $ORIGIN/../lib)
# glibc 2.41 x86_64 sonames + libgcc_s.so.1 + the built libz.so.1. No rpath surgery: RUNPATH stays.
for soname in libc.so.6 libdl.so.2 librt.so.1 libpthread.so.0 libm.so.6; do
  src=`ls "$XGLIBC/lib/$soname" 2>/dev/null | head -1`
  test -n "$src" -a -e "$src" || fail "x86_64 glibc 2.41 is missing $soname"
  cp -L "$src" "$tree/lib/$soname"
done
cp -L "$XLIBGCCDIR/libgcc_s.so.1" "$tree/lib/libgcc_s.so.1" || fail "no libgcc_s.so.1 in $XLIBGCCDIR"
cp -L "$XLIBZ" "$tree/lib/libz.so.1"
chmod -R u+w "$tree"

# --- RELINK: td's OWN elf-set-interp retargets the interpreter to /td/store/ld (no patchelf) -------
# /td/store/ld is short -> fits the original /lib64/ld-linux-x86-64.so.2 slot in-place (no growing).
for b in rustc cargo; do
  "$TB" elf-set-interp "$tree/bin/$b" /td/store/ld || fail "elf-set-interp $b"
  i=`"$TB" elf-interp "$tree/bin/$b"`
  case "$i" in /td/store/*) ;; *) fail "interp of $b not relinked to /td/store (got: $i)" ;; esac
done
echo "   [structural] rustc + cargo interp relinked to /td/store/ld (was /lib64/ld-linux-x86-64.so.2)"

# --- intern the relinked, self-contained tree at /td/store -----------------------------------------
snwork=`mktemp -d`; store="$snwork/td-store"; sndb="$snwork/store.db"; mkdir -p "$store"
out=`"$TB" store-add-recursive rust-1.96.0-x86_64-store-native "$tree" "$store" "$sndb"` || fail "store-add-recursive"
case "$out" in /td/store/*-rust-1.96.0-x86_64-store-native) ;; *) fail "interned path not content-addressed under /td/store (got: $out)" ;; esac
phys="$store/`basename "$out"`"; rustrel=${out#/td/store/}
test -x "$phys/bin/rustc" -a -x "$phys/bin/cargo" || fail "interned tree missing rustc/cargo at $phys"

# --- [no-guix] the interned /td/store rust package has zero /gnu/store anywhere --------------------
if grep -r -a -q '/gnu/store' "$phys" 2>/dev/null; then
  fail "interned rust tree contains a /gnu/store reference: `grep -r -a -l '/gnu/store' "$phys" 2>/dev/null | head -1`"
fi
echo "   [no-guix] interned $out — zero /gnu/store (rust bins upstream + td-built glibc/libgcc/libz)"

# --- [structural] the lib/ closure is COMPLETE: every soname rustc/cargo NEED is present -----------
for need in librustc_driver libLLVM libc.so.6 libdl.so.2 librt.so.1 libpthread.so.0 libm.so.6 libgcc_s.so.1 libz.so.1; do
  ls "$phys"/lib/*"$need"* >/dev/null 2>&1 || fail "the interned lib/ is missing a NEEDED lib: $need"
done
echo "   [structural] the interned lib/ holds the complete rustc/cargo runtime closure"

# --- provide /td/store/ld (the x86_64 glibc 2.41 loader) at the store root for the own-root --------
cp -L "$XGLIBC/lib/ld-linux-x86-64.so.2" "$store/ld" || fail "could not place the x86_64 loader at /td/store/ld"
! grep -q -a '/gnu/store' "$store/ld" || fail "the /td/store/ld loader contains /gnu/store bytes"

# --- a static bash (td's own store-closure reader, no guix process) for the own-root shell ---------
bashlock=`grep -- '-bash-' tests/hello-no-guix.lock | grep -v static | sed 's/^[^ ]* //' | head -1`
bs=`"$TB" store-closure /var/guix/db/db.sqlite "$bashlock" | grep -- '-bash-static-' | head -1`
test -n "$bs" -a -x "$bs/bin/bash" || fail "no static bash in hello's closure"
bbase=`basename "$bs"`; cp -a "$bs" "$store/$bbase"; chmod -R u+w "$store"

# --- RUN rustc + cargo from /td/store in the store-ns own-root (probe is a FILE — no nested quoting) -
cat > "$store/probe.sh" <<PROBE
[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
/td/store/$rustrel/bin/rustc -vV && echo RUSTC-RAN
/td/store/$rustrel/bin/cargo --version && echo CARGO-RAN
PROBE
out2=`"$TB" store-ns "$store" -- "/td/store/$bbase/bin/bash" /td/store/probe.sh 2>&1` \
  || { printf '%s\n' "$out2" | sed 's/^/     /' >&2; fail "store-ns rust run exited nonzero"; }
printf '%s\n' "$out2" | sed 's/^/     /'

# --- [behavioral] + [structural] ------------------------------------------------------------------
printf '%s\n' "$out2" | grep -q '^rustc 1\.96\.0' || fail "rustc did not print its version from the own-root"
printf '%s\n' "$out2" | grep -q '^RUSTC-RAN$'     || fail "rustc -vV did not run cleanly from /td/store"
printf '%s\n' "$out2" | grep -q '^cargo 1\.96\.0' || fail "cargo did not print its version from the own-root"
printf '%s\n' "$out2" | grep -q '^CARGO-RAN$'     || fail "cargo --version did not run cleanly from /td/store"
echo "   [behavioral] rustc -vV AND cargo --version RAN from /td/store in the store-ns own-root → rustc/cargo 1.96.0"
printf '%s\n' "$out2" | grep -q '^GNU-ABSENT$'    || fail "/gnu/store is PRESENT in the own-root — mixed with the guix install"
echo "   [structural] inside td's own root /td/store IS the store AND /gnu/store is ABSENT"

echo "PASS: rust-x86_64-runtime-store-native — from the 229-byte seed, td built the i686 chain → gcc 14.3.0,"
echo "  CROSSED UP to an x86_64 toolchain (glibc 2.41 + libgcc_s) + built x86_64 zlib, RELINKED the upstream"
echo "  Rust 1.96.0 rustc + cargo to /td/store (td's own ELF rewriter, no patchelf) with their full runtime"
echo "  closure co-located, and RAN rustc -vV + cargo --version from /td/store in the store-ns own-root →"
echo "  rustc/cargo 1.96.0, /gnu/store ABSENT. An x86_64 Rust toolchain that runs with no guix process and no"
echo "  guix bytes in its store — the rust-store-native runtime leg #196 left pending is now GREEN."
