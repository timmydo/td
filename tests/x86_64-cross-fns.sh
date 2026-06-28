#!/bin/sh
# tests/x86_64-cross-fns.sh — the x86_64 CROSS rungs of the x86_64-toolchain track, SOURCED by both
# the authoritative gate (tests/bootstrap-x86_64-toolchain-store-native.sh) and the dev harness
# (.td-build-cache/sbdev1/x86-harness.sh). Built BY the existing i686 gcc 14.3.0 + binutils 2.44
# (the modern /td/store toolchain — all i686). The cross flow (LFS / crosstool-NG shape):
#
#   cross binutils 2.44 (--target=x86_64-pc-linux-gnu)
#     -> cross gcc 14.3.0 stage1 (C only, --without-headers, all-gcc + all-target-libgcc)
#        -> x86_64 glibc 2.41 (built by the stage1 cross-cc; ld-linux-x86-64.so.2 + libc.so.6)
#           -> cross gcc 14.3.0 stage2 (c,c++ --enable-shared -> libgcc_s.so.1 + libstdc++)
#
# The cross TOOLS are i686 build tools (run in the sandbox/own-root, linked -static vs glibc 2.16);
# their OUTPUT targets x86_64 /td/store. The build-time scaffolding (awk/sed/make/bison/flex from
# the exposed /gnu/store) is guarded by the gate's [no-guix] leg (it checks the OUTPUT, not the
# build tools). Requires globals the chain defines: GCC14_TB GMP63_TB MPFR421_TB MPC131_TB BU244_TB
# GLIBC241_TB ROOT + fail().
XTARGET=x86_64-pc-linux-gnu
# The MODERN cross builds (binutils 2.44 / gcc 14 / glibc 2.41) parallelize safely — PLAN task #1
# endorses -j for exactly these (keep the mesboot base serial). Override with X86_MAKE_J= for serial.
: "${X86_MAKE_J:=-j4}"

# _store_tool <name> <guix-pkg> — a build-time scaffolding tool, from PATH or the exposed /gnu/store
_store_tool() { command -v "$1" 2>/dev/null || ls /gnu/store/*"$2"*/bin/"$1" 2>/dev/null | sort | head -1; }

# _xbin <dir> — a bin/ of the build-time tools the autoconf/recursive-make builds need (build host)
_xbin() {
  d=$1; mkdir -p "$d"
  for tool in awk:gawk gawk:gawk sed:sed grep:grep make:make m4:m4 bison:bison flex:flex \
              cmp:diffutils diff:diffutils msgfmt:gettext makeinfo:texinfo python3:python gzip:gzip; do
    n=${tool%%:*}; pk=${tool##*:}; b=`_store_tool "$n" "$pk"`; test -n "$b" && ln -sf "$b" "$d/$n" || true
  done
  ln -sf "$d/flex" "$d/lex" 2>/dev/null || true; ln -sf "$d/bison" "$d/yacc" 2>/dev/null || true
}

# _mk_static_wrapper <gcc14> <glibc216-static> <gcc|g++> <out> — a single-token, -static i686 gcc-14
# wrapper SCRIPT for compiling the BUILD/host (i686) parts of the cross builds. build_gcc_14's
# CC_FOR_BUILD trick: gcc strips trailing flags from a plain CC_FOR_BUILD on a native build, so the
# build CC must be ONE token (a script survives the munging — and -isystem/-B hide inside it). The
# glibc 2.16 headers (-idirafter) + libs/crt (-B) are the i686 libc the host conftest/programs need
# (else `fatal error: stdio.h`); gcc's own headers + libstdc++ come from gcc14 automatically. NOTE
# -idirafter, NOT -isystem: -isystem places the libc dir BEFORE gcc's built-in C++ header dirs, so
# libstdc++'s `<cstdlib>` `#include_next <stdlib.h>` (which searches AFTER its own c++ dir) never
# reaches it → `fatal error: stdlib.h`. -idirafter appends after ALL standard dirs, so #include_next
# resolves. (build_gcc_14 sidestepped this with --with-build-sysroot; a self-contained wrapper can't.)
_mk_static_wrapper() {
  g14=$1; gst=$2; which=$3; dst=$4; csh=`command -v bash 2>/dev/null || command -v sh`
  printf '#!%s\nexec "%s/bin/%s" -static -idirafter %s/include -B%s/lib "$@"\n' "$csh" "$g14" "$which" "$gst" "$gst" > "$dst"
  chmod 0555 "$dst"
}

# ---------------------------------------------------------------------------------------------------
# build_binutils_x86_64 <cpath> <gcc14> <glibc216-static> <binutils244-i686> <sysroot> <out>
#   Cross GNU Binutils 2.44 (--target=x86_64-pc-linux-gnu), built STATIC by the i686 gcc 14.3.0.
#   Output: i686 host binaries x86_64-pc-linux-gnu-{as,ld,ar,...} (in $out/bin) that EMIT x86_64.
build_binutils_x86_64() {
  cpath=$1; gcc14=$2; gst=$3; bu_i686=$4; sysroot=$5; out=$6
  rm -rf "$out"; mkdir -p "$out"
  xzb=`_store_tool xz xz-`; test -n "$xzb" || { echo "no xz" >&2; return 1; }
  csh=`command -v bash 2>/dev/null || command -v sh`
  wb=`mktemp -d`/wb; mkdir -p "$wb"; _mk_static_wrapper "$gcc14" "$gst" gcc "$wb/cc"
  tb=`mktemp -d`/tb; _xbin "$tb"
  src=`mktemp -d`/binutils; mkdir -p "$src"
  "$xzb" -dc "$BU244_TB" | tar -xf - -C "$src" --strip-components=1 || { echo "binutils-2.44 unpack failed" >&2; return 1; }
  ( cd "$src"; bp="$bu_i686/bin:$tb:$cpath"
    env PATH="$bp" CONFIG_SHELL="$csh" SHELL="$csh" CC="$wb/cc" CC_FOR_BUILD="$wb/cc" AR="$bu_i686/bin/ar" RANLIB="$bu_i686/bin/ranlib" \
      "$csh" ./configure --build=i686-pc-linux-gnu --host=i686-pc-linux-gnu --target=$XTARGET \
      --prefix=/td/store/binutils-2.44-x86_64 --with-sysroot="$sysroot" \
      --disable-nls --disable-gold --disable-werror --enable-deterministic-archives \
      --disable-plugins --disable-gprofng --disable-multilib >cfg.log 2>&1 \
      || { echo "x86_64 binutils configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_xbu-cfg.log" 2>/dev/null||true; tail -25 cfg.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" SHELL="$csh" make $X86_MAKE_J MAKEINFO=true >build.log 2>&1 \
      || { echo "x86_64 binutils make failed" >&2; cp build.log "$ROOT/.td-build-cache/_xbu-build.log" 2>/dev/null||true; tail -30 build.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" SHELL="$csh" make MAKEINFO=true install prefix="$out" >inst.log 2>&1 \
      || { echo "x86_64 binutils install failed" >&2; tail -20 inst.log >&2; return 1; } ) || return 1
  test -x "$out/bin/$XTARGET-as" -a -x "$out/bin/$XTARGET-ld" || { echo "no x86_64 as/ld produced" >&2; return 1; }
}

# ---------------------------------------------------------------------------------------------------
# build_gcc_x86_64_stage1 <cpath> <gcc14> <glibc216-static> <binutils244-i686> <xbu> <sysroot> <out>
#   Cross GCC 14.3.0 stage1: C only, --without-headers, --disable-shared. `make all-gcc
#   all-target-libgcc` only (no libc yet). Built STATIC by the i686 gcc 14. Produces
#   x86_64-pc-linux-gnu-gcc (i686 binary emitting x86_64) + a minimal libgcc.a — enough to build glibc.
build_gcc_x86_64_stage1() {
  cpath=$1; gcc14=$2; gst=$3; bu_i686=$4; xbu=$5; sysroot=$6; out=$7
  rm -rf "$out"; mkdir -p "$out"
  xzb=`_store_tool xz xz-`; test -n "$xzb" || { echo "no xz" >&2; return 1; }
  csh=`command -v bash 2>/dev/null || command -v sh`
  wb=`mktemp -d`/wb; mkdir -p "$wb"
  _mk_static_wrapper "$gcc14" "$gst" gcc "$wb/cc"; _mk_static_wrapper "$gcc14" "$gst" g++ "$wb/cxx"
  tb=`mktemp -d`/tb; _xbin "$tb"
  src=`mktemp -d`/gcc; mkdir -p "$src"
  "$xzb" -dc "$GCC14_TB" | tar -xf - -C "$src" --strip-components=1 || { echo "gcc-14.3.0 unpack failed" >&2; return 1; }
  "$xzb" -dc "$GMP63_TB" | tar -xf - -C "$src" || { echo "gmp unpack failed" >&2; return 1; }
  "$xzb" -dc "$MPFR421_TB" | tar -xf - -C "$src" || { echo "mpfr unpack failed" >&2; return 1; }
  tar -xzf "$MPC131_TB" -C "$src" || { echo "mpc unpack failed" >&2; return 1; }
  ( cd "$src" && ln -sf gmp-6.3.0 gmp && ln -sf mpfr-4.2.1 mpfr && ln -sf mpc-1.3.1 mpc ) || return 1
  ( cd "$src"; bp="$xbu/bin:$bu_i686/bin:$tb:$cpath"
    for f in `grep -rl '^#! */bin/sh' . 2>/dev/null`; do sed -i "1s,^#! *[^ ]*/bin/sh,#!$csh," "$f" 2>/dev/null || true; done
    rm -rf bld; mkdir bld; cd bld
    env PATH="$bp" CONFIG_SHELL="$csh" CC="$wb/cc" CXX="$wb/cxx" CPP="$wb/cc -E" CC_FOR_BUILD="$wb/cc" CXX_FOR_BUILD="$wb/cxx" \
        "$csh" ../configure --build=i686-pc-linux-gnu --host=i686-pc-linux-gnu --target=$XTARGET \
        --prefix=/td/store/gcc-14.3.0-x86_64 --with-sysroot="$sysroot" \
        --enable-languages=c --without-headers --with-newlib --with-glibc-version=2.41 \
        --disable-bootstrap --disable-multilib --disable-shared --disable-threads \
        --disable-libssp --disable-libgomp --disable-libquadmath --disable-libatomic \
        --disable-libvtv --disable-libitm --disable-libstdcxx --disable-libcc1 \
        --disable-lto --disable-plugin --disable-decimal-float --disable-werror >cfg.log 2>&1 \
      || { echo "x86_64 gcc stage1 configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_xgcc1-cfg.log" 2>/dev/null||true; tail -25 cfg.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" \
        make $X86_MAKE_J SHELL="$csh" MAKEINFO=true all-gcc all-target-libgcc >build.log 2>&1 \
      || { echo "x86_64 gcc stage1 make failed" >&2; cp build.log "$ROOT/.td-build-cache/_xgcc1-build.log" 2>/dev/null||true; tail -40 build.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" \
        make SHELL="$csh" MAKEINFO=true install-gcc install-target-libgcc DESTDIR="$out/stage" >inst.log 2>&1 \
      || { echo "x86_64 gcc stage1 install failed" >&2; tail -20 inst.log >&2; return 1; } ) || return 1
  test -x "$out/stage/td/store/gcc-14.3.0-x86_64/bin/$XTARGET-gcc" || { echo "no x86_64 stage1 gcc produced" >&2; return 1; }
}

# ---------------------------------------------------------------------------------------------------
# build_glibc_x86_64 <cpath> <gcc14> <glibc216-static> <xbu> <xgcc1> <sysroot> <kh_x86_64_tb> <out>
#   MODERN glibc 2.41 for x86_64, built by the stage1 cross-gcc. CC=<x86_64-cross-gcc>,
#   BUILD_CC=<i686 gcc14 wrapper> (cross build: build-time helpers run on i686). Produces a SHARED
#   x86_64 libc: ld-linux-x86-64.so.2 + libc.so.6. Interned at /td/store/glibc-2.41-x86_64.
build_glibc_x86_64() {
  cpath=$1; gcc14=$2; gst=$3; xbu=$4; xgcc1=$5; sysroot=$6; kh=$7; out=$8
  rm -rf "$out"; mkdir -p "$out"
  xzb=`_store_tool xz xz-`; test -n "$xzb" || { echo "no xz" >&2; return 1; }
  csh=`command -v bash 2>/dev/null || command -v sh`
  bwb=`mktemp -d`/bwb; mkdir -p "$bwb"; _mk_static_wrapper "$gcc14" "$gst" gcc "$bwb/cc"   # i686 BUILD_CC
  tb=`mktemp -d`/tb; _xbin "$tb"
  xgccbin="$xgcc1/stage/td/store/gcc-14.3.0-x86_64/bin"
  src=`mktemp -d`/glibc; mkdir -p "$src"
  "$xzb" -dc "$GLIBC241_TB" | tar -xf - -C "$src" --strip-components=1 || { echo "glibc-2.41 unpack failed" >&2; return 1; }
  ( cd "$src"
    for f in `grep -rl '^#! */bin/sh' . 2>/dev/null`; do sed -i "1s,^#! *[^ ]*/bin/sh,#!$csh," "$f" 2>/dev/null || true; done
    sed -i "s,^SHELL := /bin/sh,SHELL := $csh," Makeconfig 2>/dev/null || true
    rm -rf bld; mkdir bld; cd bld
    env PATH="$xgccbin:$xbu/bin:$tb:$cpath" CONFIG_SHELL="$csh" SHELL="$csh" \
        CC="$XTARGET-gcc" BUILD_CC="$bwb/cc" \
        AR="$XTARGET-ar" RANLIB="$XTARGET-ranlib" \
        "$csh" ../configure --prefix=/td/store/glibc-2.41-x86_64 \
        --build=i686-pc-linux-gnu --host=$XTARGET \
        --with-headers="$sysroot/usr/include" --enable-kernel=3.2.0 --disable-werror --disable-nscd \
        --with-binutils="$xbu/bin" libc_cv_slibdir=/td/store/glibc-2.41-x86_64/lib >cfg.log 2>&1 \
      || { echo "x86_64 glibc configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_xglibc-cfg.log" 2>/dev/null||true; tail -30 cfg.log >&2; return 1; }
    env PATH="$xgccbin:$xbu/bin:$tb:$cpath" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" SHELL="$csh" \
        make $X86_MAKE_J >build.log 2>&1 \
      || { echo "x86_64 glibc make failed" >&2; cp build.log "$ROOT/.td-build-cache/_xglibc-build.log" 2>/dev/null||true; tail -40 build.log >&2; return 1; }
    env PATH="$xgccbin:$xbu/bin:$tb:$cpath" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" SHELL="$csh" \
        make install DESTDIR="$out/stage" >inst.log 2>&1 \
      || { echo "x86_64 glibc install failed" >&2; tail -20 inst.log >&2; return 1; } ) || return 1
  gl="$out/stage/td/store/glibc-2.41-x86_64"
  test -e "$gl/lib/libc.so.6" -a -e "$gl/lib/ld-linux-x86-64.so.2" || { echo "no x86_64 libc.so.6/ld.so produced" >&2; return 1; }
  # relocate glibc's ld scripts (libc.so/libpthread.so): strip the configure prefix to bare names.
  for so in "$gl/lib/"*.so; do
    if head -c20 "$so" 2>/dev/null | grep -q 'GNU ld script' 2>/dev/null; then
      sed -i "s,/td/store/glibc-2.41-x86_64/lib/,,g" "$so"
    fi
  done
  # populate the sysroot so the stage2 cross-gcc finds the x86_64 glibc at build time. MERGE the glibc
  # headers INTO the existing kernel headers ($sysroot/usr/include — glibc headers #include <linux/…>,
  # <asm/…>), and copy the glibc libs + crt + loader. ld scripts already relocated to bare names above.
  mkdir -p "$sysroot/usr/include" "$sysroot/usr/lib"
  cp -a "$gl/include/." "$sysroot/usr/include/"
  cp -a "$gl/lib/." "$sysroot/usr/lib/"
  rm -f "$sysroot/lib"; ln -sf usr/lib "$sysroot/lib"
}

# ---------------------------------------------------------------------------------------------------
# build_gcc_x86_64_stage2 <cpath> <gcc14> <glibc216-static> <binutils244-i686> <xbu> <sysroot> <out>
#   Cross GCC 14.3.0 stage2 (full): c,c++ --enable-shared against the x86_64 glibc sysroot ->
#   libgcc_s.so.1 (rustc needs it dynamically) + libstdc++.so.6 + the x86_64 cross-gcc/g++.
build_gcc_x86_64_stage2() {
  cpath=$1; gcc14=$2; gst=$3; bu_i686=$4; xbu=$5; sysroot=$6; out=$7
  rm -rf "$out"; mkdir -p "$out"
  xzb=`_store_tool xz xz-`; test -n "$xzb" || { echo "no xz" >&2; return 1; }
  csh=`command -v bash 2>/dev/null || command -v sh`
  wb=`mktemp -d`/wb; mkdir -p "$wb"
  _mk_static_wrapper "$gcc14" "$gst" gcc "$wb/cc"; _mk_static_wrapper "$gcc14" "$gst" g++ "$wb/cxx"
  tb=`mktemp -d`/tb; _xbin "$tb"
  src=`mktemp -d`/gcc; mkdir -p "$src"
  "$xzb" -dc "$GCC14_TB" | tar -xf - -C "$src" --strip-components=1 || { echo "gcc-14.3.0 unpack failed" >&2; return 1; }
  "$xzb" -dc "$GMP63_TB" | tar -xf - -C "$src" || { echo "gmp unpack failed" >&2; return 1; }
  "$xzb" -dc "$MPFR421_TB" | tar -xf - -C "$src" || { echo "mpfr unpack failed" >&2; return 1; }
  tar -xzf "$MPC131_TB" -C "$src" || { echo "mpc unpack failed" >&2; return 1; }
  ( cd "$src" && ln -sf gmp-6.3.0 gmp && ln -sf mpfr-4.2.1 mpfr && ln -sf mpc-1.3.1 mpc ) || return 1
  ( cd "$src"; bp="$xbu/bin:$bu_i686/bin:$tb:$cpath"
    for f in `grep -rl '^#! */bin/sh' . 2>/dev/null`; do sed -i "1s,^#! *[^ ]*/bin/sh,#!$csh," "$f" 2>/dev/null || true; done
    rm -rf bld; mkdir bld; cd bld
    env PATH="$bp" CONFIG_SHELL="$csh" CC="$wb/cc" CXX="$wb/cxx" CPP="$wb/cc -E" CC_FOR_BUILD="$wb/cc" CXX_FOR_BUILD="$wb/cxx" \
        "$csh" ../configure --build=i686-pc-linux-gnu --host=i686-pc-linux-gnu --target=$XTARGET \
        --prefix=/td/store/gcc-14.3.0-x86_64 --with-sysroot="$sysroot" \
        --enable-languages=c,c++ --enable-shared --enable-threads=posix --enable-c99 --with-glibc-version=2.41 \
        --disable-bootstrap --disable-multilib --disable-libssp --disable-libgomp \
        --disable-libquadmath --disable-libvtv --disable-libitm --disable-libcc1 \
        --disable-libsanitizer --disable-lto --disable-plugin --disable-decimal-float \
        --disable-libstdcxx-pch --disable-werror >cfg.log 2>&1 \
      || { echo "x86_64 gcc stage2 configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_xgcc2-cfg.log" 2>/dev/null||true; tail -25 cfg.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" \
        make $X86_MAKE_J SHELL="$csh" MAKEINFO=true >build.log 2>&1 \
      || { echo "x86_64 gcc stage2 make failed" >&2; cp build.log "$ROOT/.td-build-cache/_xgcc2-build.log" 2>/dev/null||true; tail -40 build.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" \
        make SHELL="$csh" MAKEINFO=true install DESTDIR="$out/stage" >inst.log 2>&1 \
      || { echo "x86_64 gcc stage2 install failed" >&2; tail -20 inst.log >&2; return 1; } ) || return 1
  g="$out/stage/td/store/gcc-14.3.0-x86_64"
  test -x "$g/bin/$XTARGET-gcc" -a -x "$g/bin/$XTARGET-g++" || { echo "no x86_64 stage2 gcc/g++ produced" >&2; return 1; }
  find "$g" -name 'libgcc_s.so.1' | head -1 | grep -q . || { echo "x86_64 stage2 did not produce libgcc_s.so.1" >&2; return 1; }
}

# ---------------------------------------------------------------------------------------------------
# run_x86_64_cross <cpath> <gcc14> <glibc216-static> <glibc216-shared> <binutils244-i686> <kh_x86_64_tb>
#   Build all four cross rungs (into mktemp dirs) + a fast harness verify (run the x86_64 program via
#   the explicit loader on the x86_64 host). Exports XGLIBC / XGCC2 / XLIBGCC for the caller's own
#   verify. The gate calls the build_* functions itself and does the store-ns own-root verify.
run_x86_64_cross() {
  cpath=$1; gcc14=$2; gst=$3; gshared=$4; bu_i686=$5; kh=$6
  work=`mktemp -d`/x86; mkdir -p "$work"
  sysroot="$work/sysroot"; mkdir -p "$sysroot/usr/include"
  tar -xzf "$kh" -C "$sysroot/usr/include" || { echo "x86_64 kernel headers unpack failed" >&2; return 1; }

  echo ">> [x1] cross binutils 2.44 (--target=$XTARGET)"
  XBU="$work/binutils"; build_binutils_x86_64 "$cpath" "$gcc14" "$gst" "$bu_i686" "$sysroot" "$XBU" || return 1
  echo ">> [x2] cross gcc 14.3.0 stage1 (C, no libc)"
  XGCC1="$work/gcc1"; build_gcc_x86_64_stage1 "$cpath" "$gcc14" "$gst" "$bu_i686" "$XBU" "$sysroot" "$XGCC1" || return 1
  echo ">> [x3] x86_64 glibc 2.41 (built by the stage1 cross-gcc)"
  XGLIBCB="$work/glibc"; build_glibc_x86_64 "$cpath" "$gcc14" "$gst" "$XBU" "$XGCC1" "$sysroot" "$kh" "$XGLIBCB" || return 1
  echo ">> [x4] cross gcc 14.3.0 stage2 (c,c++ + shared libgcc_s)"
  XGCC2B="$work/gcc2"; build_gcc_x86_64_stage2 "$cpath" "$gcc14" "$gst" "$bu_i686" "$XBU" "$sysroot" "$XGCC2B" || return 1

  XGLIBC="$XGLIBCB/stage/td/store/glibc-2.41-x86_64"
  XGCC2="$XGCC2B/stage/td/store/gcc-14.3.0-x86_64"
  XLIBGCC=`find "$XGCC2" -name 'libgcc_s.so.1' | head -1`; XLIBGCCDIR=`dirname "$XLIBGCC"`
  XSTDCXXDIR=`find "$XGCC2" -name 'libstdc++.so.6' | head -1 | xargs -r dirname`

  echo ">> [verify] compile an x86_64 C + C++ program and run it via the x86_64 loader"
  w="$work/w"; mkdir -p "$w"
  printf 'int main(){return 42;}\n' > "$w/c.c"
  printf '#include <vector>\nint main(){std::vector<int> v; for(int i=0;i<43;i++) v.push_back(i); return v[42];}\n' > "$w/cpp.cc"
  csh=`command -v bash 2>/dev/null || command -v sh`
  bw="$work/bw"; mkdir -p "$bw"; rel="glibc-2.41-x86_64"   # logical /td/store name (harness uses the live dir)
  for cc in gcc g++; do
    printf '#!%s\nexec "%s/bin/%s-%s" -isystem "%s/include" -B"%s/lib" -L"%s/lib" -L"%s" -L"%s" -Wl,--dynamic-linker -Wl,/td/store/%s/lib/ld-linux-x86-64.so.2 -Wl,-rpath -Wl,/td/store/%s/lib "$@"\n' \
      "$csh" "$XGCC2" "$XTARGET" "$cc" "$XGLIBC" "$XGLIBC" "$XGLIBC" "$XLIBGCCDIR" "$XSTDCXXDIR" "$rel" "$rel" > "$bw/$cc"
  done
  chmod 0555 "$bw/gcc" "$bw/g++"
  ( cd "$w" && env PATH="$XBU/bin:$cpath" "$bw/gcc" -o c.out c.c ) || { echo "x86_64 C compile failed" >&2; return 1; }
  ( cd "$w" && env PATH="$XBU/bin:$cpath" "$bw/g++" -O2 -o cpp.out cpp.cc ) || { echo "x86_64 C++ compile failed" >&2; return 1; }
  cls=`"$XBU/bin/$XTARGET-readelf" -h "$w/c.out" 2>/dev/null | grep -i 'class:' | grep -o 'ELF64'`
  test "$cls" = ELF64 || { echo "the C program is not ELF64 (x86_64): got '$cls'" >&2; return 1; }
  ci=`"$XBU/bin/$XTARGET-readelf" -l "$w/c.out" 2>/dev/null | grep -o '/td/store/[^]]*ld-linux-x86-64.so.2' | head -1`
  test -n "$ci" || { echo "the C program interp is not the /td/store x86_64 ld" >&2; return 1; }
  # run via the explicit loader on the x86_64 host (the baked /td/store interp doesn't exist here)
  crc=`"$XGLIBC/lib/ld-linux-x86-64.so.2" --library-path "$XGLIBC/lib:$XLIBGCCDIR:$XSTDCXXDIR" "$w/c.out"; echo $?`
  cpprc=`"$XGLIBC/lib/ld-linux-x86-64.so.2" --library-path "$XGLIBC/lib:$XLIBGCCDIR:$XSTDCXXDIR" "$w/cpp.out"; echo $?`
  test "$crc" = 42 || { echo "x86_64 C program returned $crc (want 42)" >&2; return 1; }
  test "$cpprc" = 42 || { echo "x86_64 C++ program returned $cpprc (want 42)" >&2; return 1; }
  echo "PASS-HARNESS-CROSS: x86_64 C ($crc) + C++ ($cpprc) built by the cross gcc 14.3.0, ELF64, interp=$ci, run via the /td/store x86_64 glibc 2.41 loader"
  # leave $work in place for the caller to inspect / intern
  X86_WORK="$work"; export X86_WORK XGLIBC XGCC2 XLIBGCCDIR XSTDCXXDIR XBU
}
