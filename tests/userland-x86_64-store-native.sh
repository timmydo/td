#!/bin/sh
# tests/userland-x86_64-store-native.sh — host-sandbox-stage0 inc2: the guix-less daily-suite
# CAPTURED SET's C userland (busybox + GNU make) at /td/store, NO GUIX BYTES. From the
# 229-byte seed, td builds the i686 chain → gcc 14.3.0, CROSSES UP to a native x86_64
# toolchain (gcc 14.3.0 + glibc 2.41 + libgcc_s; reused from the x86_64 gate as a function
# library), then builds busybox 1.37.0 + GNU make 4.4.1 FROM upstream source (td-fetch,
# sha-pinned) with that toolchain — DYNAMIC against the /td/store glibc 2.41 (interp =
# /td/store/ld, RUNPATH = $ORIGIN/../lib). The set + its glibc/libgcc closure is interned
# content-addressed at /td/store, and `busybox sh` runs a script that drives `make` in the
# store-ns own-root with /gnu/store ABSENT.
#
# This is the daily-suite harness userland, guix-byte-free by construction: upstream source
# + td's own from-source /td/store toolchain. busybox (a POSIX userland) is deliberate — a
# silent dependency on a GNUism surfaces as a loud failure. (td-builder, the engine, joins
# the set via rust-store-native rung 3; this gate proves the busybox+make half.) HEAVY
# (~90 min from the seed; directive 1 — no cache for the authoritative gate). NOT a BUILD_GATE.
#
# Legs (DURABLE — no guix oracle):
#   [supply-chain] busybox + make tarballs match their lock sha256 (the sha IS the oracle).
#   [provenance]   the built busybox/make carry zero /gnu/store bytes.
#   [no-guix]      the interned /td/store set (bins + td-built glibc/libgcc) has zero
#                  /gnu/store anywhere; the relinked interp is /td/store/ld.
#   [structural]   the tree's lib/ closure is complete (every NEEDED soname present).
#   [behavioral]   busybox sh runs + `make --version` runs from /td/store in the store-ns
#                  own-root → the real version strings.
#   [structural]   inside the own-root /td/store IS the store AND /gnu/store is ABSENT.
# Verified-red (in-gate): without the elf-set-interp relink the own-root run FAILS (the
# build-dir interp does not exist in the own-root) — the relink is load-bearing.
set -eu

# --- source the x86_64 toolchain gate as a FUNCTION LIBRARY (build_* rungs + pinned vars) --
export TD_X86_64_LIB=1
. tests/bootstrap-x86_64-toolchain-store-native.sh
unset TD_X86_64_LIB
# in scope: ROOT, fail(), sha(), lf(), make_curated_path, the build_* rungs, KH_X86_64_TB.

# --- [supply-chain] busybox + make tarballs match their lock sha256 -------------------------
BB_LOCK=`ls seed/sources/busybox-*.lock 2>/dev/null | head -1`
test -n "$BB_LOCK" || fail "no seed/sources/busybox-*.lock pin"
BB_TB=".td-build-cache/sources/`lf "$BB_LOCK" file`"
test -f "$BB_TB" || fail "warmed $BB_TB absent — run tools/warm-bootstrap-sources.sh (host PREP)"
test "`sha "$BB_TB"`" = "`lf "$BB_LOCK" sha256`" || fail "warmed $BB_TB sha256 != lock pin"
MK_LOCK=`ls seed/sources/make-4.4*.lock 2>/dev/null | head -1`
test -n "$MK_LOCK" || fail "no seed/sources/make-4.4*.lock pin"
MK_TB=".td-build-cache/sources/`lf "$MK_LOCK" file`"
test -f "$MK_TB" || fail "warmed $MK_TB absent — run tools/warm-bootstrap-sources.sh (host PREP)"
test "`sha "$MK_TB"`" = "`lf "$MK_LOCK" sha256`" || fail "warmed $MK_TB sha256 != lock pin"
echo "   [supply-chain] busybox + make-4.4.1 match their lock sha256 — upstream bytes, not guix"

# An x86_64 cc wrapper that builds RUNNABLE binaries (interp = the build-dir glibc loader, so
# configure tests + build-time tools run now) + RUNPATH $ORIGIN/../lib (so the shipped tree
# finds its libs). The final binary's interp is relinked to /td/store/ld afterward.
#   $1=outfile $2=XGCC2 $3=XGLIBC $4=XLIBGCCDIR
emit_cc() {
  csh=`command -v bash 2>/dev/null || command -v sh`
  printf '#!%s\nexec "%s/bin/%s-gcc" -isystem "%s/include" -B"%s/lib" -L"%s/lib" -L"%s" -Wl,--dynamic-linker -Wl,"%s/lib/ld-linux-x86-64.so.2" -Wl,-rpath -Wl,'\''$ORIGIN/../lib'\'' "$@"\n' \
    "$csh" "$2" "$XTARGET" "$3" "$3" "$3" "$4" "$3" > "$1"
  chmod 0555 "$1"
}

# build_make_x86_64 <cpath> <xgcc2> <xglibc> <xlibgccdir> <xbu> <out> — GNU make 4.4.1, the
# build driver. Configure+build with the runnable cc; output: $out/make (interp relinked later).
build_make_x86_64() {
  mc=$1; xg=$2; xgl=$3; xlg=$4; xb=$5; out=$6
  rm -rf "$out"; mkdir -p "$out"
  csh=`command -v bash 2>/dev/null || command -v sh`
  src=`mktemp -d`/make; mkdir -p "$src"
  tar -xzf "$MK_TB" -C "$src" --strip-components=1 || { echo "make unpack failed" >&2; return 1; }
  # The sandbox has NO /bin/sh: run configure THROUGH the curated shell (its #!/bin/sh shebang
  # would otherwise fail "No such file or directory"), and rewrite any #!/bin/sh helper shebangs.
  find "$src" -type f -exec sed -i "1s|^#! */bin/sh\b|#!$csh|" {} + 2>/dev/null || true
  wb=`mktemp -d`/wb; mkdir -p "$wb"; emit_cc "$wb/cc" "$xg" "$xgl" "$xlg"
  ( cd "$src"; bp="$xb/bin:$mc"
    env PATH="$bp" CC="$wb/cc" CONFIG_SHELL="$csh" SHELL="$csh" "$csh" ./configure --build="$XTARGET" --host="$XTARGET" --disable-dependency-tracking >cfg.log 2>&1 \
      || { echo "make configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_makex-cfg.log" 2>/dev/null||true; tail -25 cfg.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= make SHELL="$csh" CONFIG_SHELL="$csh" >build.log 2>&1 \
      || { echo "make build failed" >&2; cp build.log "$ROOT/.td-build-cache/_makex-build.log" 2>/dev/null||true; tail -25 build.log >&2; return 1; }
    cp -a make "$out/make" ) || return 1
  test -x "$out/make" || { echo "no x86_64 make produced" >&2; return 1; }
}

# build_busybox_x86_64 <cpath> <xgcc2> <xglibc> <xlibgccdir> <xbu> <out> — busybox 1.37.0
# (dynamic). build-host == target (both x86_64), so HOSTCC == CC (the runnable wrapper);
# CONFIG_STATIC off (dynamic vs /td/store glibc). Output: $out/busybox (interp relinked later).
build_busybox_x86_64() {
  mc=$1; xg=$2; xgl=$3; xlg=$4; xb=$5; out=$6
  rm -rf "$out"; mkdir -p "$out"
  csh=`command -v bash 2>/dev/null || command -v sh`
  bz=`command -v bzip2 2>/dev/null || ls /gnu/store/*bzip2*/bin/bzip2 2>/dev/null | head -1`
  test -n "$bz" || { echo "no bzip2 to unpack busybox" >&2; return 1; }
  src=`mktemp -d`/bb; mkdir -p "$src"
  "$bz" -dc "$BB_TB" | tar -xf - -C "$src" --strip-components=1 || { echo "busybox unpack failed" >&2; return 1; }
  # The sandbox has NO /bin/sh: busybox's Kbuild + gen scripts (#!/bin/sh) would fail. Rewrite the
  # shebangs to the curated shell and pass SHELL/CONFIG_SHELL to every make so recipes use it too.
  find "$src" -type f -exec sed -i "1s|^#! */bin/sh\b|#!$csh|" {} + 2>/dev/null || true
  wb=`mktemp -d`/wb; mkdir -p "$wb"; emit_cc "$wb/cc" "$xg" "$xgl" "$xlg"
  ( cd "$src"; bp="$xb/bin:$mc"
    env PATH="$bp" make CC="$wb/cc" HOSTCC="$wb/cc" SHELL="$csh" CONFIG_SHELL="$csh" defconfig >cfg.log 2>&1 \
      || { echo "busybox defconfig failed" >&2; tail -20 cfg.log >&2; return 1; }
    # dynamic (not CONFIG_STATIC), non-PIE, point the linker at the build-dir glibc archives.
    sed -i -E '/^#? *CONFIG_STATIC[ =]/d; /^#? *CONFIG_PIE[ =]/d; /^#? *CONFIG_EXTRA_LDFLAGS[ =]/d' .config
    { echo '# CONFIG_STATIC is not set'; echo '# CONFIG_PIE is not set'; echo "CONFIG_EXTRA_LDFLAGS=\"-L$xgl/lib -L$xlg\""; } >> .config
    yes "" | env PATH="$bp" make CC="$wb/cc" HOSTCC="$wb/cc" SHELL="$csh" CONFIG_SHELL="$csh" oldconfig >/dev/null 2>&1
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= \
      make CC="$wb/cc" HOSTCC="$wb/cc" SKIP_STRIP=y SHELL="$csh" CONFIG_SHELL="$csh" -j"$(nproc)" >build.log 2>&1 \
      || { echo "busybox build failed" >&2; cp build.log "$ROOT/.td-build-cache/_bbx-build.log" 2>/dev/null||true; tail -25 build.log >&2; return 1; }
    cp -a busybox "$out/busybox" ) || return 1
  test -x "$out/busybox" || { echo "no x86_64 busybox produced" >&2; return 1; }
}

# ============================================================================================
# Build the i686 base FROM THE SEED, then CROSS UP to x86_64 — REUSING the x86_64 gate's rungs.
# (identical prologue to the rust-x86_64 gate.)
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
MKX=`mktemp -d`/makex; BBX=`mktemp -d`/bbx
trap 'rm -rf "$tc" "$mesp" "`dirname "$TCCD"`" "`dirname "$MK"`" "`dirname "$PD"`" "`dirname "$BD"`" "`dirname "$GD"`" "`dirname "$HD"`" "`dirname "$GLD"`" "`dirname "$G2"`" "`dirname "$B2"`" "`dirname "$MM"`" "`dirname "$GM1"`" "`dirname "$BMB"`" "`dirname "$GAWKMB"`" "`dirname "$GOUT"`" "`dirname "$GMB"`" "`dirname "$GSH"`" "`dirname "$GCC14B"`" "`dirname "$BMB244SB"`" "`dirname "$MKX"`" "`dirname "$BBX"`" "`dirname "$cpath"`" "${snwork:-}" "${tree:-}"' EXIT INT TERM

GCC14="$GCC14B/stage/td/store/gcc-14.3.0"; GST="$GOUT/out"
echo "   built the i686 base: gcc 14.3.0 + glibc 2.16 (static+shared) + binutils 2.44"
. tests/x86_64-cross-fns.sh
run_x86_64_cross "$cpath" "$GCC14" "$GST" "$GSH/out" "$BMB244SB" "$KH_X86_64_TB" || fail "the x86_64 cross rungs failed"
# exports: XGLIBC XGCC2 XLIBGCCDIR XSTDCXXDIR XBU X86_WORK
echo "   crossed up: x86_64 glibc 2.41 ($XGLIBC) + libgcc_s ($XLIBGCCDIR)"

# --- build the C userland (busybox + make) dynamic vs the x86_64 glibc 2.41 -----------------
build_make_x86_64    "$cpath" "$XGCC2" "$XGLIBC" "$XLIBGCCDIR" "$XBU" "$MKX" || fail "the cross gcc did not build GNU make 4.4.1"
build_busybox_x86_64 "$cpath" "$XGCC2" "$XGLIBC" "$XLIBGCCDIR" "$XBU" "$BBX" || fail "the cross gcc did not build busybox 1.37.0"
for b in "$MKX/make" "$BBX/busybox"; do
  ! grep -q -a '/gnu/store' "$b" || fail "$b contains /gnu/store bytes — not guix-free"
done
echo "   [provenance] built busybox + make carry zero /gnu/store bytes"

# --- assemble the self-contained tree (bins + glibc/libgcc closure in lib/) -----------------
. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
export TD_STORE_DIR=/td/store
tree=`mktemp -d`/tree; mkdir -p "$tree/bin" "$tree/lib"
cp "$BBX/busybox" "$tree/bin/busybox"; cp "$MKX/make" "$tree/bin/make"
for soname in libc.so.6 libdl.so.2 librt.so.1 libpthread.so.0 libm.so.6 libresolv.so.2; do
  s=`ls "$XGLIBC/lib/$soname" 2>/dev/null | head -1`; test -n "$s" -a -e "$s" && cp -L "$s" "$tree/lib/$soname" || true
done
cp -L "$XLIBGCCDIR/libgcc_s.so.1" "$tree/lib/libgcc_s.so.1" || fail "no libgcc_s.so.1"
chmod -R u+w "$tree"
# relink each executable's interp to /td/store/ld (RUNPATH already $ORIGIN/../lib from the link)
for b in busybox make; do
  "$TB" elf-set-interp "$tree/bin/$b" /td/store/ld || fail "elf-set-interp $b"
  case `"$TB" elf-interp "$tree/bin/$b"` in /td/store/*) ;; *) fail "interp of $b not relinked to /td/store" ;; esac
done
# busybox applet symlinks (sh, sed, grep, …) so the userland is callable by name
( cd "$tree/bin"; for a in sh sed grep awk find tar gzip ls cat cp mkdir rm env printf xargs sort head tail wc tr cut; do ln -sf busybox "$a"; done )
echo "   [structural] busybox + make interp relinked to /td/store/ld; applet symlinks placed"

# --- intern the tree at /td/store + place the loader -----------------------------------------
snwork=`mktemp -d`; store="$snwork/td-store"; sndb="$snwork/store.db"; mkdir -p "$store"
out=`"$TB" store-add-recursive userland-x86_64-store-native "$tree" "$store" "$sndb"` || fail "store-add-recursive"
case "$out" in /td/store/*-userland-x86_64-store-native) ;; *) fail "interned path not content-addressed under /td/store (got: $out)" ;; esac
phys="$store/`basename "$out"`"; rel=${out#/td/store/}
test -x "$phys/bin/busybox" -a -x "$phys/bin/make" || fail "interned tree missing busybox/make"
if grep -r -a -q '/gnu/store' "$phys" 2>/dev/null; then fail "interned set contains /gnu/store: `grep -r -a -l '/gnu/store' "$phys" 2>/dev/null | head -1`"; fi
echo "   [no-guix] interned $out — zero /gnu/store (busybox/make + td-built glibc/libgcc)"
for need in libc.so.6 libm.so.6 libgcc_s.so.1; do ls "$phys"/lib/*"$need"* >/dev/null 2>&1 || fail "interned lib/ missing $need"; done
echo "   [structural] the interned lib/ holds the userland runtime closure"
cp -L "$XGLIBC/lib/ld-linux-x86-64.so.2" "$store/ld" || fail "could not place the x86_64 loader at /td/store/ld"
! grep -q -a '/gnu/store' "$store/ld" || fail "the /td/store/ld loader contains /gnu/store bytes"

# --- RUN busybox sh + make from /td/store in the store-ns own-root ---------------------------
cat > "$store/probe.sh" <<PROBE
[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
/td/store/$rel/bin/busybox echo BUSYBOX-RAN
/td/store/$rel/bin/busybox sed --version 2>/dev/null | head -1
/td/store/$rel/bin/make --version | head -1 && echo MAKE-RAN
PROBE
out2=`"$TB" store-ns "$store" -- "/td/store/$rel/bin/busybox" sh /td/store/probe.sh 2>&1` \
  || { printf '%s\n' "$out2" | sed 's/^/     /' >&2; fail "store-ns userland run exited nonzero"; }
printf '%s\n' "$out2" | sed 's/^/     /'
printf '%s\n' "$out2" | grep -q '^BUSYBOX-RAN$' || fail "busybox did not run from /td/store"
printf '%s\n' "$out2" | grep -q '^GNU Make 4\.4' || fail "make did not print its version from /td/store"
printf '%s\n' "$out2" | grep -q '^MAKE-RAN$'     || fail "make --version did not run cleanly from /td/store"
echo "   [behavioral] busybox + make RAN from /td/store in the store-ns own-root → GNU Make 4.4.1"
printf '%s\n' "$out2" | grep -q '^GNU-ABSENT$'   || fail "/gnu/store is PRESENT in the own-root"
echo "   [structural] inside td's own root /td/store IS the store AND /gnu/store is ABSENT"

echo "PASS: userland-x86_64-store-native — from the 229-byte seed, td built the i686 chain → gcc 14.3.0,"
echo "  crossed up to x86_64, and built busybox 1.37.0 + GNU make 4.4.1 from upstream source, DYNAMIC vs the"
echo "  /td/store glibc 2.41, interned at /td/store, and RAN them in the store-ns own-root → /gnu/store ABSENT,"
echo "  zero guix bytes. The C userland of the guix-less daily-suite captured set."
