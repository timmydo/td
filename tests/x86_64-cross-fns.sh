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
  g14=$1; gst=$2; which=$3; dst=$4; extra=${5:-}; csh=`command -v bash 2>/dev/null || command -v sh`
  printf '#!%s\nexec "%s/bin/%s" -static -idirafter %s/include -B%s/lib %s "$@"\n' "$csh" "$g14" "$which" "$gst" "$gst" "$extra" > "$dst"
  chmod 0555 "$dst"
}

# _x86_stable_tooldir <xbu> — REPRODUCIBILITY (gcc14-repro / [[toolchain-repro]]). Stage the cross
# binutils' x86_64 as/ld at a FIXED, deterministic path so the cross gcc bakes a STABLE
# DEFAULT_ASSEMBLER/DEFAULT_LINKER. `--with-as`/`--with-ld` are AC_DEFINE'd into gcc/gcc.cc and live
# in the gcc DRIVER binary's `.rodata` (find_a_program: `access(DEFAULT_ASSEMBLER, X_OK)==0`), NOT in
# DWARF — so repro_normalize_tree's `--strip-debug` can NOT scrub them. Pointing them at the per-build
# mktemp $xbu (the old code) made the driver differ byte-for-byte every build. The baked path is DEAD
# at runtime (the access() guard + #225's tooldir bundle resolve as/ld relative to argv[0]); it only
# has to (1) satisfy access(X_OK) at BUILD time and (2) be the SAME string build-to-build. A fixed-name
# dir of symlinks to the live $xbu as/ld does both. The dir is a FIXED ABSOLUTE /tmp path (NOT
# $ROOT-relative), so the baked DEFAULT_ASSEMBLER string is identical for EVERY builder regardless of
# where the worktree is checked out — checkout-independent, like guix's /tmp/guix-build-* dirs. The
# loop's host-sandbox gives each run a private tmpfs /tmp (builder/src/sandbox.rs), so the fixed name is
# isolated per run and vanishes on teardown. Echoes the stable dir.
_x86_stable_tooldir() {
  _xbu=$1
  _d="/tmp/td-x86_64-with-as"
  mkdir -p "$_d"
  ln -sf "$_xbu/bin/$XTARGET-as" "$_d/$XTARGET-as"
  ln -sf "$_xbu/bin/$XTARGET-ld" "$_d/$XTARGET-ld"
  echo "$_d"
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
  wb=/tmp/td-x86_64-wrapper; rm -rf "$wb"; mkdir -p "$wb"; _mk_static_wrapper "$gcc14" "$gst" gcc "$wb/cc"
  tb=/tmp/td-x86_64-tools; rm -rf "$tb"; _xbin "$tb"   # FIXED path (gcc14-repro): $tb leaks into fixincl's baked SED path (.rodata) — a per-build mktemp made fixincl differ build-to-build
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
  # FIXED path (gcc14-repro): the host-wrapper dir $wb is $(LINKER) in gcc's `checksum-options`, which
  # genchecksum hashes into the cc1/cc1plus executable_checksum (.rodata) — a per-build mktemp made that
  # 16-byte MD5 differ build-to-build (the residual non-determinism strip can't scrub). Fixed /tmp path.
  wb=/tmp/td-x86_64-wrapper; rm -rf "$wb"; mkdir -p "$wb"
  # REPRODUCIBILITY (gcc14-repro): a deterministic -frandom-seed for the HOST compiler building cc1/
  # cc1plus/fixincl. gcc's get_file_function_name (tree.cc) names file-scope static initializers
  # `<src>_<crc32>_<random_seed>`; with -frandom-seed UNSET gcc reads /dev/urandom (toplev.cc) → those
  # symbols (kept in .symtab past `strip --strip-debug`) differ build-to-build. gcc's own bootstrap
  # passes -frandom-seed=$@; --disable-bootstrap skips that, so we pin it here. SAFE as a single fixed
  # value: the per-TU `<src>` prefix keeps the symbols unique across TUs (no ODR clash); LTO is off.
  # Also pins local_tick -> -1 (deterministic DWARF timestamps). Bonus: stage1 reproducible too.
  _mk_static_wrapper "$gcc14" "$gst" gcc "$wb/cc" -frandom-seed=tdgcc14repro; _mk_static_wrapper "$gcc14" "$gst" g++ "$wb/cxx" -frandom-seed=tdgcc14repro
  tb=/tmp/td-x86_64-tools; rm -rf "$tb"; _xbin "$tb"   # FIXED path (gcc14-repro): $tb leaks into fixincl's baked SED path (.rodata) — a per-build mktemp made fixincl differ build-to-build
  # REPRODUCIBILITY (gcc14-repro): a FIXED source/build dir, NOT a per-build mktemp, so the absolute
  # build path baked into cc1/cc1plus/fixincl (.symtab STT_FILE + __FILE__ in .rodata — NOT DWARF, so
  # `strip --strip-debug` does NOT scrub it) is deterministic build-to-build. A FIXED ABSOLUTE /tmp path
  # (NOT $ROOT-relative) so the baked build path is identical for EVERY builder regardless of checkout
  # location — checkout-independent, like guix's /tmp/guix-build-* dir. The host-sandbox gives each run a
  # private tmpfs /tmp, so the fixed name is isolated per run and vanishes on teardown (no worktree-disk
  # leak). Two independent cross-gcc builds (the repro leg) then differ only in DWARF (stripped) + archive
  # mtimes (-D) → byte-identical after normalization. rm'd fresh each call; sequential rungs never overlap.
  src="/tmp/td-x86_64-gcc14-src"; rm -rf "$src"; mkdir -p "$src"
  "$xzb" -dc "$GCC14_TB" | tar -xf - -C "$src" --strip-components=1 || { echo "gcc-14.3.0 unpack failed" >&2; return 1; }
  "$xzb" -dc "$GMP63_TB" | tar -xf - -C "$src" || { echo "gmp unpack failed" >&2; return 1; }
  "$xzb" -dc "$MPFR421_TB" | tar -xf - -C "$src" || { echo "mpfr unpack failed" >&2; return 1; }
  tar -xzf "$MPC131_TB" -C "$src" || { echo "mpc unpack failed" >&2; return 1; }
  ( cd "$src" && ln -sf gmp-6.3.0 gmp && ln -sf mpfr-4.2.1 mpfr && ln -sf mpc-1.3.1 mpc ) || return 1
  wadir=`_x86_stable_tooldir "$xbu"`   # deterministic --with-as/--with-ld (gcc14-repro)
  ( cd "$src"; bp="$xbu/bin:$bu_i686/bin:$tb:$cpath"
    for f in `grep -rl '^#! */bin/sh' . 2>/dev/null`; do sed -i "1s,^#! *[^ ]*/bin/sh,#!$csh," "$f" 2>/dev/null || true; done
    rm -rf bld; mkdir bld; cd bld
    env PATH="$bp" CONFIG_SHELL="$csh" CC="$wb/cc" CXX="$wb/cxx" CPP="$wb/cc -E" CC_FOR_BUILD="$wb/cc" CXX_FOR_BUILD="$wb/cxx" \
        "$csh" ../configure --build=i686-pc-linux-gnu --host=i686-pc-linux-gnu --target=$XTARGET \
        --prefix=/td/store/gcc-14.3.0-x86_64 --with-sysroot="$sysroot" \
        --with-as="$wadir/$XTARGET-as" --with-ld="$wadir/$XTARGET-ld" \
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
  bwb=/tmp/td-x86_64-bwrapper; rm -rf "$bwb"; mkdir -p "$bwb"; _mk_static_wrapper "$gcc14" "$gst" gcc "$bwb/cc"   # i686 BUILD_CC (FIXED path, gcc14-repro)
  tb=/tmp/td-x86_64-tools; rm -rf "$tb"; _xbin "$tb"   # FIXED path (gcc14-repro): $tb leaks into fixincl's baked SED path (.rodata) — a per-build mktemp made fixincl differ build-to-build
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
        --with-binutils="$xbu/$XTARGET/bin" libc_cv_slibdir=/td/store/glibc-2.41-x86_64/lib >cfg.log 2>&1 \
      || { echo "x86_64 glibc configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_xglibc-cfg.log" 2>/dev/null||true; cp config.log "$ROOT/.td-build-cache/_xglibc-config.log" 2>/dev/null||true; tail -30 cfg.log >&2; return 1; }
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
  # FIXED path (gcc14-repro): the host-wrapper dir $wb is $(LINKER) in gcc's `checksum-options`, which
  # genchecksum hashes into the cc1/cc1plus executable_checksum (.rodata) — a per-build mktemp made that
  # 16-byte MD5 differ build-to-build (the residual non-determinism strip can't scrub). Fixed /tmp path.
  wb=/tmp/td-x86_64-wrapper; rm -rf "$wb"; mkdir -p "$wb"
  # REPRODUCIBILITY (gcc14-repro): a deterministic -frandom-seed for the HOST compiler building cc1/
  # cc1plus/fixincl. gcc's get_file_function_name (tree.cc) names file-scope static initializers
  # `<src>_<crc32>_<random_seed>`; with -frandom-seed UNSET gcc reads /dev/urandom (toplev.cc) → those
  # symbols (kept in .symtab past `strip --strip-debug`) differ build-to-build. gcc's own bootstrap
  # passes -frandom-seed=$@; --disable-bootstrap skips that, so we pin it here. SAFE as a single fixed
  # value: the per-TU `<src>` prefix keeps the symbols unique across TUs (no ODR clash); LTO is off.
  # Also pins local_tick -> -1 (deterministic DWARF timestamps). Bonus: stage1 reproducible too.
  _mk_static_wrapper "$gcc14" "$gst" gcc "$wb/cc" -frandom-seed=tdgcc14repro; _mk_static_wrapper "$gcc14" "$gst" g++ "$wb/cxx" -frandom-seed=tdgcc14repro
  tb=/tmp/td-x86_64-tools; rm -rf "$tb"; _xbin "$tb"   # FIXED path (gcc14-repro): $tb leaks into fixincl's baked SED path (.rodata) — a per-build mktemp made fixincl differ build-to-build
  # REPRODUCIBILITY (gcc14-repro): a FIXED source/build dir, NOT a per-build mktemp, so the absolute
  # build path baked into cc1/cc1plus/fixincl (.symtab STT_FILE + __FILE__ in .rodata — NOT DWARF, so
  # `strip --strip-debug` does NOT scrub it) is deterministic build-to-build. A FIXED ABSOLUTE /tmp path
  # (NOT $ROOT-relative) so the baked build path is identical for EVERY builder regardless of checkout
  # location — checkout-independent, like guix's /tmp/guix-build-* dir. The host-sandbox gives each run a
  # private tmpfs /tmp, so the fixed name is isolated per run and vanishes on teardown (no worktree-disk
  # leak). Two independent cross-gcc builds (the repro leg) then differ only in DWARF (stripped) + archive
  # mtimes (-D) → byte-identical after normalization. rm'd fresh each call; sequential rungs never overlap.
  src="/tmp/td-x86_64-gcc14-src"; rm -rf "$src"; mkdir -p "$src"
  "$xzb" -dc "$GCC14_TB" | tar -xf - -C "$src" --strip-components=1 || { echo "gcc-14.3.0 unpack failed" >&2; return 1; }
  "$xzb" -dc "$GMP63_TB" | tar -xf - -C "$src" || { echo "gmp unpack failed" >&2; return 1; }
  "$xzb" -dc "$MPFR421_TB" | tar -xf - -C "$src" || { echo "mpfr unpack failed" >&2; return 1; }
  tar -xzf "$MPC131_TB" -C "$src" || { echo "mpc unpack failed" >&2; return 1; }
  ( cd "$src" && ln -sf gmp-6.3.0 gmp && ln -sf mpfr-4.2.1 mpfr && ln -sf mpc-1.3.1 mpc ) || return 1
  wadir=`_x86_stable_tooldir "$xbu"`   # deterministic --with-as/--with-ld (gcc14-repro)
  ( cd "$src"; bp="$xbu/bin:$bu_i686/bin:$tb:$cpath"
    for f in `grep -rl '^#! */bin/sh' . 2>/dev/null`; do sed -i "1s,^#! *[^ ]*/bin/sh,#!$csh," "$f" 2>/dev/null || true; done
    rm -rf bld; mkdir bld; cd bld
    env PATH="$bp" CONFIG_SHELL="$csh" CC="$wb/cc" CXX="$wb/cxx" CPP="$wb/cc -E" CC_FOR_BUILD="$wb/cc" CXX_FOR_BUILD="$wb/cxx" \
        "$csh" ../configure --build=i686-pc-linux-gnu --host=i686-pc-linux-gnu --target=$XTARGET \
        --prefix=/td/store/gcc-14.3.0-x86_64 --with-sysroot="$sysroot" \
        --with-as="$wadir/$XTARGET-as" --with-ld="$wadir/$XTARGET-ld" \
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
  # X86_RUNG_CACHE (dev harness only): reuse the cross binutils + gcc stage1 (the unchanging early rungs)
  # across runs, with a STABLE sysroot, so glibc/stage2 iterations skip ~20 min. The from-seed GATE
  # leaves it UNSET → every rung builds fresh in $work (directive 1).
  rc="${X86_RUNG_CACHE:-}"
  # REPRODUCIBILITY (gcc14-repro): a FIXED sysroot path even off the dev cache, so the cross gcc bakes
  # a STABLE --with-sysroot (TARGET_SYSTEM_ROOT in the driver .rodata) + a stable include-fixed (gcc
  # copies fixed system headers referencing the sysroot path) — a per-build mktemp $work/sysroot made
  # the cross gcc differ build-to-build in bytes strip can't scrub. The gate path is a FIXED ABSOLUTE
  # /tmp dir (NOT $ROOT-relative) so the baked sysroot string is checkout-independent (private tmpfs /tmp
  # per sandbox-run; vanishes on teardown). rm'd fresh below each run.
  if [ -n "$rc" ]; then sysroot="$rc/x-sysroot"; else sysroot="/tmp/td-x86_64-sysroot"; fi
  rm -rf "$sysroot"; mkdir -p "$sysroot/usr/include"
  tar -xzf "$kh" -C "$sysroot/usr/include" || { echo "x86_64 kernel headers unpack failed" >&2; return 1; }

  echo ">> [x1] cross binutils 2.44 (--target=$XTARGET)"
  if [ -n "$rc" ] && [ -x "$rc/x-binutils/bin/$XTARGET-as" ]; then
    XBU="$rc/x-binutils"; echo "   (reusing cached cross binutils)"
  else
    if [ -n "$rc" ]; then XBU="$rc/x-binutils"; else XBU="$work/binutils"; fi
    build_binutils_x86_64 "$cpath" "$gcc14" "$gst" "$bu_i686" "$sysroot" "$XBU" || return 1
  fi
  echo ">> [x2] cross gcc 14.3.0 stage1 (C, no libc)"
  if [ -n "$rc" ] && [ -x "$rc/x-gcc1/stage/td/store/gcc-14.3.0-x86_64/bin/$XTARGET-gcc" ]; then
    XGCC1="$rc/x-gcc1"; echo "   (reusing cached cross gcc stage1)"
  else
    if [ -n "$rc" ]; then XGCC1="$rc/x-gcc1"; else XGCC1="$work/gcc1"; fi
    build_gcc_x86_64_stage1 "$cpath" "$gcc14" "$gst" "$bu_i686" "$XBU" "$sysroot" "$XGCC1" || return 1
  fi
  echo ">> [x3] x86_64 glibc 2.41 (built by the stage1 cross-gcc)"
  if [ -n "$rc" ] && [ -e "$rc/x-glibc/stage/td/store/glibc-2.41-x86_64/lib/libc.so.6" ]; then
    XGLIBCB="$rc/x-glibc"; echo "   (reusing cached x86_64 glibc)"
  else
    if [ -n "$rc" ]; then XGLIBCB="$rc/x-glibc"; else XGLIBCB="$work/glibc"; fi
    build_glibc_x86_64 "$cpath" "$gcc14" "$gst" "$XBU" "$XGCC1" "$sysroot" "$kh" "$XGLIBCB" || return 1
  fi
  echo ">> [x4] cross gcc 14.3.0 stage2 (c,c++ + shared libgcc_s)"
  if [ -n "$rc" ] && [ -x "$rc/x-gcc2/stage/td/store/gcc-14.3.0-x86_64/bin/$XTARGET-g++" ]; then
    XGCC2B="$rc/x-gcc2"; echo "   (reusing cached cross gcc stage2)"
  else
    if [ -n "$rc" ]; then XGCC2B="$rc/x-gcc2"; else XGCC2B="$work/gcc2"; fi
    build_gcc_x86_64_stage2 "$cpath" "$gcc14" "$gst" "$bu_i686" "$XBU" "$sysroot" "$XGCC2B" || return 1
  fi

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
  X86_WORK="$work"; X86_SYSROOT="$sysroot"; export X86_WORK X86_SYSROOT XGLIBC XGCC2 XLIBGCCDIR XSTDCXXDIR XBU
}

# ---------------------------------------------------------------------------------------------------
# verify_x86_64_ownroot <cpath> <scratch> — the gate's DURABLE own-root verify, shared with the dev
# harness. Interns the x86_64 glibc 2.41 at /td/store, builds x86_64 C/C++ verify programs (interp =
# the interned /td/store x86_64 ld-linux-x86-64.so.2, -static-libgcc -static-libstdc++ so the own-root
# needs only the interned glibc), and RUNS them in the store-ns own-root → 42 with /gnu/store ABSENT.
# Requires: $TB (caller load_stage0'd), TD_STORE_DIR=/td/store, and the run_x86_64_cross exports
# (XGLIBC XGCC2 XBU). Legs: [no-guix] [content-addr] [behavioral] [structural] [input-addressed]
# (the lock-keyed path a consumer fetches as a substitute — x64-toolchain-subst PR2).
verify_x86_64_ownroot() {
  cpath=$1; snwork=$2; store="$snwork/td-store"; sndb="$snwork/store.db"; mkdir -p "$store"
  xcc1=`find "$XGCC2" -name cc1 | head -1`
  for b in "$XGLIBC/lib/libc.so.6" "$XGCC2/bin/$XTARGET-gcc" "$xcc1"; do
    test -n "$b" -a -e "$b" || { echo "x86_64 output missing ($b)" >&2; return 1; }
    if grep -q -a '/gnu/store' "$b"; then echo "$b contains /gnu/store bytes" >&2; return 1; fi
  done
  echo "   [no-guix] x86_64 glibc 2.41 + cross gcc: no /gnu/store in libc.so.6 / x86_64-gcc / cc1"
  GLP=`"$TB" store-add-recursive glibc-2.41-x86_64 "$XGLIBC" "$store" "$sndb"` || { echo "store-add x86_64 glibc failed" >&2; return 1; }
  case "$GLP" in /td/store/*-glibc-2.41-x86_64) ;; *) echo "x86_64 glibc not content-addressed: $GLP" >&2; return 1 ;; esac
  glrel=${GLP#/td/store/}
  echo "   [content-addr] interned $GLP in /td/store"
  csh=`command -v bash 2>/dev/null || command -v sh`
  mkdir -p "$snwork/w"
  printf 'int main(){return 42;}\n' > "$snwork/w/c.c"
  printf '#include <vector>\nint main(){std::vector<int> v; for(int i=0;i<43;i++) v.push_back(i); return v[42];}\n' > "$snwork/w/cpp.cc"
  bw=`mktemp -d`/bw; mkdir -p "$bw"
  for cc in gcc g++; do
    printf '#!%s\nexec "%s/bin/%s-%s" -isystem "%s/include" -B"%s/lib" -L"%s/lib" -static-libgcc -static-libstdc++ -Wl,--dynamic-linker -Wl,/td/store/%s/lib/ld-linux-x86-64.so.2 -Wl,--enable-new-dtags -Wl,-rpath -Wl,/td/store/%s/lib "$@"\n' \
      "$csh" "$XGCC2" "$XTARGET" "$cc" "$XGLIBC" "$XGLIBC" "$XGLIBC" "$glrel" "$glrel" > "$bw/$cc"
  done
  chmod 0555 "$bw/gcc" "$bw/g++"
  ( cd "$snwork/w" && env PATH="$XBU/bin:$cpath" "$bw/gcc" -o c.out c.c ) || { echo "cross gcc did not compile x86_64 C vs glibc 2.41" >&2; return 1; }
  ( cd "$snwork/w" && env PATH="$XBU/bin:$cpath" "$bw/g++" -O2 -o cpp.out cpp.cc ) || { echo "cross g++ did not compile x86_64 C++ vs glibc 2.41" >&2; return 1; }
  cls=`"$XBU/bin/$XTARGET-readelf" -h "$snwork/w/c.out" 2>/dev/null | grep -i 'class:' | grep -o 'ELF64'`
  test "$cls" = ELF64 || { echo "verify program not ELF 64-bit (x86_64); got '$cls'" >&2; return 1; }
  ci=`"$XBU/bin/$XTARGET-readelf" -l "$snwork/w/c.out" 2>/dev/null | grep -o "/td/store/$glrel/lib/ld-linux-x86-64.so.2" | head -1`
  test -n "$ci" || { echo "C program interp not the /td/store x86_64 ld" >&2; return 1; }
  if grep -q -a '/gnu/store' "$snwork/w/c.out"; then echo "x86_64 C program contains /gnu/store bytes" >&2; return 1; fi
  echo "   built x86_64 (ELF 64-bit) C + C++ programs vs glibc 2.41, interp=$ci, no /gnu/store"
  mkdir -p "$store/prog/bin"; cp "$snwork/w/c.out" "$store/prog/bin/c"; cp "$snwork/w/cpp.out" "$store/prog/bin/cpp"; chmod -R u+w "$store"
  WP=`"$TB" store-add-recursive prog "$store/prog" "$store" "$sndb"` || { echo "store-add prog failed" >&2; return 1; }; wprel=${WP#/td/store/}
  bashlock=`grep -- '-bash-' tests/hello-no-guix.lock | grep -v static | sed 's/^[^ ]* //' | head -1`
  bs=`"$TB" store-closure /var/guix/db/db.sqlite "$bashlock" | grep -- '-bash-static-' | head -1`
  bbase=`basename "$bs"`; cp -a "$bs" "$store/$bbase"; chmod -R u+w "$store"
  snscript='[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
/td/store/'"$wprel"'/bin/c; echo "CRC=$?"
/td/store/'"$wprel"'/bin/cpp; echo "CPPRC=$?"'
  snout=`"$TB" store-ns "$store" -- "/td/store/$bbase/bin/bash" -c "$snscript" 2>&1` || { printf '%s\n' "$snout" | sed 's/^/     /' >&2; echo "store-ns x86_64 probe exited nonzero" >&2; return 1; }
  printf '%s\n' "$snout" | sed 's/^/     /' >&2
  echo "$snout" | grep -q '^CRC=42$'   || { echo "x86_64 C program did not return 42 in the own-root" >&2; return 1; }
  echo "$snout" | grep -q '^CPPRC=42$' || { echo "x86_64 C++ program did not return 42 in the own-root" >&2; return 1; }
  echo "   [behavioral] cross gcc 14.3.0 links a DYNAMIC x86_64 C AND C++ program vs MODERN x86_64 glibc 2.41; both run in the own-root → 42"
  echo "$snout" | grep -q '^GNU-ABSENT$' || { echo "/gnu/store is PRESENT in the own-root" >&2; return 1; }
  echo "   [structural] inside td's own root /td/store IS the store AND /gnu/store is ABSENT"

  # --- [input-addressed] (x64-toolchain-subst) intern the REAL x86_64 glibc 2.41 at the
  # LOCK-KEYED path so a consumer can NAME it and FETCH it as a signed substitute (the path
  # td-subst / resolve-toolchain.sh compute from tests/td-toolchain-x86_64.lock) instead of the
  # ~90-min cross rebuild — real x86_64 bytes at a stable, predictable /td/store path, not a
  # content-addressed throwaway. Then RUN a DYNAMIC x86_64 program whose interp IS that
  # input-addressed glibc. Gate 418 (toolchain-x86_64-input-addressed, #219) keys the path with a
  # static-bash FIXTURE; this leg ties the path to the REAL cross-built x86_64 toolchain bytes.
  XLOCK=tests/td-toolchain-x86_64.lock
  test -f "$XLOCK" || { echo "missing $XLOCK" >&2; return 1; }
  XK=`"$TB" toolchain-key "$XLOCK"` || { echo "toolchain-key $XLOCK failed" >&2; return 1; }
  IAGL=`"$TB" store-add-input-addressed glibc-2.41-x86_64 "$XK" "$XGLIBC" "$store" "$sndb"` \
    || { echo "store-add-input-addressed x86_64 glibc failed" >&2; return 1; }
  WANTGL=`"$TB" toolchain-path "$XLOCK" glibc-2.41-x86_64`
  test "$IAGL" = "$WANTGL" || { echo "input-addressed glibc path $IAGL != lock-computed $WANTGL (consumer can't predict it)" >&2; return 1; }
  # x64 focus: the x86_64 toolchain must NOT share a /td/store path with the i686 bootstrap
  # intermediate, or a published x86_64 substitute could be confused for i686.
  ILGL=`"$TB" toolchain-path tests/td-toolchain.lock glibc-2.41`
  test -n "$ILGL" -a "$IAGL" != "$ILGL" || { echo "x86_64 glibc path $IAGL collides with the i686 glibc path $ILGL" >&2; return 1; }
  echo "   [distinct-arch] the x86_64 lock-keyed path differs from the i686 toolchain's — no cross-arch store collision"
  iarel=${IAGL#/td/store/}
  echo "   [input-addressed] interned the REAL x86_64 glibc 2.41 at the lock-keyed path $IAGL (== toolchain-path $XLOCK glibc-2.41-x86_64)"
  printf '#!%s\nexec "%s/bin/%s-gcc" -isystem "%s/include" -B"%s/lib" -L"%s/lib" -static-libgcc -static-libstdc++ -Wl,--dynamic-linker -Wl,/td/store/%s/lib/ld-linux-x86-64.so.2 -Wl,--enable-new-dtags -Wl,-rpath -Wl,/td/store/%s/lib "$@"\n' \
    "$csh" "$XGCC2" "$XTARGET" "$XGLIBC" "$XGLIBC" "$XGLIBC" "$iarel" "$iarel" > "$bw/gcc-ia"
  chmod 0555 "$bw/gcc-ia"
  ( cd "$snwork/w" && env PATH="$XBU/bin:$cpath" "$bw/gcc-ia" -o cia.out c.c ) \
    || { echo "could not build an x86_64 C program vs the input-addressed glibc" >&2; return 1; }
  iaci=`"$XBU/bin/$XTARGET-readelf" -l "$snwork/w/cia.out" 2>/dev/null | grep -o "/td/store/$iarel/lib/ld-linux-x86-64.so.2" | head -1`
  test -n "$iaci" || { echo "input-addressed program interp not the lock-keyed /td/store x86_64 ld" >&2; return 1; }
  mkdir -p "$store/progia/bin"; cp "$snwork/w/cia.out" "$store/progia/bin/c"; chmod -R u+w "$store"
  WPIA=`"$TB" store-add-recursive progia "$store/progia" "$store" "$sndb"` || { echo "store-add progia failed" >&2; return 1; }; wpiarel=${WPIA#/td/store/}
  snia='[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
/td/store/'"$wpiarel"'/bin/c; echo "IARC=$?"'
  snoia=`"$TB" store-ns "$store" -- "/td/store/$bbase/bin/bash" -c "$snia" 2>&1` \
    || { printf '%s\n' "$snoia" | sed 's/^/     /' >&2; echo "store-ns input-addressed x86_64 probe exited nonzero" >&2; return 1; }
  echo "$snoia" | grep -q '^IARC=42$' || { printf '%s\n' "$snoia" | sed 's/^/     /' >&2; echo "x86_64 program vs the input-addressed glibc did not return 42 in the own-root" >&2; return 1; }
  echo "   [behavioral/input-addressed] a DYNAMIC x86_64 program whose interp IS the lock-keyed /td/store glibc runs in the own-root → 42 — real x86_64 bytes at a predictable, fetchable path"

}

# ---------------------------------------------------------------------------------------------------
# x86_64_gcc_repro_leg <cpath> <gcc14> <gst> <bu_i686> <xbu> <sysroot> <treeA>
#   DURABLE intrinsic double-build reproducibility (gcc14-repro / [[toolchain-repro]]) for the cross
#   gcc 14.3.0 — the assertion that retires nothing when guix leaves (no guix oracle in the room).
#   <treeA> is the freshly-built cross gcc stage2 tree (already `x86_64_bundle_tooldir`'d), NOT yet
#   normalized. This:
#     1. records treeA's RAW nar-hash, then NORMALIZES treeA in place (repro_normalize_tree: strip the
#        build-path-bearing DWARF + deterministic archives + drop *.la) so the caller interns the
#        REPRODUCIBLE bytes;
#     2. builds a SECOND, independent stage2 (fresh build dir; same fixed `--with-as`/`--with-ld`/
#        `--with-sysroot` STRINGS, but an INDEPENDENT binutils PATH — see below), bundles its tooldir,
#        records its RAW hash, normalizes it;
#     3. LOGS RAW(A) vs RAW(B) (observed self-discrimination, NOT a hard assert): the gcc build dir
#        leaks into DWARF so the raw builds normally differ, which shows the normalization below is
#        load-bearing — but a raw-identical build is a happy surprise, not a failure, so it is not asserted;
#     4. asserts NORM(A) == NORM(B) — the cross gcc is BYTE-REPRODUCIBLE: a stable, fetchable artifact.
#   strip = the cross binutils `$XTARGET-strip` (static i686; its x86-family BFD strips BOTH the i686
#   host driver/cc1 ELFs AND the x86_64 target libgcc archives), run natively (no loader wrapper).
#   Build B is configured against an INDEPENDENT binutils PATH (a cheap `cp` of $xbu to a fresh dir,
#   identical bytes) so that `--with-as`/`--with-ld` WOULD differ between A and B without the fixed
#   _x86_stable_tooldir path — i.e. reverting the fix REDS this leg (verified-red), and a green here is
#   a real cross-run reproducibility result, not a vacuous same-inputs rebuild. (--with-sysroot is
#   pinned upstream in run_x86_64_cross to a fixed path; both builds use the same $sysroot string.)
x86_64_gcc_repro_leg() {
  _cpath=$1; _gcc14=$2; _gst=$3; _bu=$4; _xbu=$5; _sysroot=$6; _treeA=$7
  _out2=""; _xbu2=""
  # Clean up build B's scratch on EVERY exit path (its gcc tree + binutils copy can be GBs, and /tmp is
  # a RAM-backed tmpfs), and re-point the shared tooldir symlinks at the live build-A binutils so build
  # B's (now-deleted) $_xbu2 leaves no dangling links behind.
  _repro_cleanup() {
    [ -n "$_out2" ] && rm -rf "${_out2%/*}" 2>/dev/null
    [ -n "$_xbu2" ] && rm -rf "${_xbu2%/*}" 2>/dev/null
    _x86_stable_tooldir "$_xbu" >/dev/null 2>&1 || true
    return 0
  }
  _strip="$_xbu/bin/$XTARGET-strip"
  test -x "$_strip" || { echo "repro: no cross strip ($_strip)" >&2; return 1; }
  _rawA=`"$TB" nar-hash "$_treeA"` || { echo "repro: nar-hash (raw) of build A failed" >&2; return 1; }
  repro_normalize_tree "$_treeA" "$_strip" || { echo "repro: normalization of build A failed" >&2; return 1; }
  _normA=`"$TB" nar-hash "$_treeA"` || { echo "repro: nar-hash (normalized) of build A failed" >&2; return 1; }
  echo "   [repro] cross gcc 14.3.0 build A: raw nar-hash $_rawA → normalized $_normA"
  # An INDEPENDENT binutils path for build B (identical bytes, different absolute path): without the
  # deterministic --with-as fix this alone makes the two gcc drivers diverge.
  _xbu2=`mktemp -d`/xbinutils2
  cp -a "$_xbu" "$_xbu2" || { echo "repro: could not stage an independent binutils path for build B" >&2; _repro_cleanup; return 1; }
  _out2=`mktemp -d`/gcc2-repro
  echo ">> [repro] second, independent cross gcc 14.3.0 stage2 build (fresh build dir + INDEPENDENT binutils path)"
  build_gcc_x86_64_stage2 "$_cpath" "$_gcc14" "$_gst" "$_bu" "$_xbu2" "$_sysroot" "$_out2" \
    || { echo "repro: the second cross gcc stage2 build did not run" >&2; _repro_cleanup; return 1; }
  _treeB="$_out2/stage/td/store/gcc-14.3.0-x86_64"
  x86_64_bundle_tooldir "$_treeB" || { echo "repro: bundle_tooldir on build B failed" >&2; _repro_cleanup; return 1; }
  _rawB=`"$TB" nar-hash "$_treeB"` || { echo "repro: nar-hash (raw) of build B failed" >&2; _repro_cleanup; return 1; }
  repro_normalize_tree "$_treeB" "$_strip" || { echo "repro: normalization of build B failed" >&2; _repro_cleanup; return 1; }
  _normB=`"$TB" nar-hash "$_treeB"` || { echo "repro: nar-hash (normalized) of build B failed" >&2; _repro_cleanup; return 1; }
  # Observed self-discrimination (logged, not asserted — a raw-reproducible build would be a happy
  # surprise, not a failure): the RAW builds differ because the gcc build dir leaks into DWARF, so the
  # normalization below is load-bearing. NORM(A)==NORM(B) is NOT vacuous: if strip were a no-op AND the
  # raw builds differ, the normalized hashes would still differ → the assertion would red.
  if [ "$_rawA" != "$_rawB" ]; then
    echo "   [repro/self-discrimination] the RAW double-build DIFFERS ($_rawA != $_rawB) — the build dir leaks into DWARF, so normalization is load-bearing"
  else
    echo "   [repro] note: the RAW double-build was already byte-identical ($_rawA) — the build is reproducible even pre-normalization"
  fi
  if [ "$_normA" != "$_normB" ]; then
    echo "repro: cross gcc 14.3.0 is NOT byte-reproducible — normalized buildA=$_normA buildB=$_normB" >&2
    # Diagnostic (only on failure): which files still differ after normalization? (no diffutils in the
    # sandbox → compare per-file sha256 via sort|uniq -u). The differing relative paths point at the
    # residual non-determinism to fix (e.g. an install-tools config / include-fixed text leak).
    ( cd "$_treeA" && find . -type f -exec sha256sum {} + 2>/dev/null | sort ) > "$ROOT/.td-build-cache/_gcc14repro-A.list" 2>/dev/null || true
    ( cd "$_treeB" && find . -type f -exec sha256sum {} + 2>/dev/null | sort ) > "$ROOT/.td-build-cache/_gcc14repro-B.list" 2>/dev/null || true
    echo "repro: files differing after normalization (sha256 + path, each appears once per build):" >&2
    sort "$ROOT/.td-build-cache/_gcc14repro-A.list" "$ROOT/.td-build-cache/_gcc14repro-B.list" | uniq -u | sed 's/^/     /' >&2 || true
    # Preserve the differing binaries from BOTH builds so the residual non-determinism can be diffed
    # (readelf/cmp) AFTER the run, instead of a blind re-build.
    _dd="$ROOT/.td-build-cache/_gcc14repro-diff"; rm -rf "$_dd"; mkdir -p "$_dd/A" "$_dd/B"
    # NOTE: the sandbox has NO awk — extract the path field with sed (sha256sum = "<hash>  <path>").
    sort "$ROOT/.td-build-cache/_gcc14repro-A.list" "$ROOT/.td-build-cache/_gcc14repro-B.list" | uniq -u | sed 's/^[^ ]*  *//' | sort -u | while read -r _rel; do
      mkdir -p "$_dd/A/`dirname "$_rel"`" "$_dd/B/`dirname "$_rel"`" 2>/dev/null || true
      cp -a "$_treeA/$_rel" "$_dd/A/$_rel" 2>/dev/null || true
      cp -a "$_treeB/$_rel" "$_dd/B/$_rel" 2>/dev/null || true
    done
    echo "repro: preserved the differing binaries under $_dd/{A,B} for post-run inspection" >&2
    _repro_cleanup
    return 1
  fi
  _repro_cleanup
  echo "   [repro] two independent from-source builds of the cross gcc 14.3.0, normalized, are byte-identical (nar-hash $_normA) — a STABLE input-addressed /td/store artifact (durable: intrinsic double-build reproducibility, no guix oracle)"
}

# ===================================================================================================
# RUNG X2 — a NATIVE x86_64 toolchain at /td/store (x86_64-toolchain track, after the #201 cross rungs).
# X1 produced a CROSS gcc: an i686 (ELF 32-bit) binary that EMITS x86_64. X2 turns that into a NATIVE
# x86_64 gcc — gcc/cc1/g++ that are themselves ELF 64-bit x86_64, run natively on x86_64, and compile
# x86_64 (host == target). Built BY the cross toolchain (XGCC2/XBU) vs the /td/store x86_64 glibc 2.41,
# STATIC (like the i686 build_gcc_14 / build_binutils_244 rungs) so the binaries run in the store-ns
# own-root with no interp dependency. The same `int main(){return 42;}` proof, but the COMPILER that
# builds + runs it is itself an x86_64 binary living in /td/store — the architectural self-hosting rung
# (a from-source gcc-rebuilds-gcc bootstrap is a separate, much heavier milestone, not claimed here).
# ---------------------------------------------------------------------------------------------------

# _mk_native_static_wrapper <cross-cc-or-c++> <glibc> <dst> [hdrdir] — a single-token CC wrapper for the
# native x86_64 builds: the cross gcc/g++, supplying the x86_64 glibc crt/libs (-B), that adds -static
# for EXECUTABLES + conftests (so they run with no interp in the own-root) but DROPS -static when the
# link is `-shared` (libtool building a shared module, e.g. binutils' ld libdep.la). On x86_64 a -static
# non-PIC crt in a shared object is an R_X86_64_32 error; the cross binutils' i686-host builds never hit
# this (R_386 allows non-PIC text relocs), so this guard is x86_64-specific. The wrapper, not make-level
# LDFLAGS, is the reliable lever (binutils' recursive program links don't honor a make LDFLAGS override).
# Optional [hdrdir] is added with -idirafter (NOT -isystem / C_INCLUDE_PATH): the headers must come AFTER
# gcc's own C++ dirs so libstdc++'s <cstdlib> `#include_next <stdlib.h>` resolves (same reason as
# _mk_static_wrapper) — a host C++ compile (cc1plus/libcody) else dies on `fatal error: stdlib.h`.
_mk_native_static_wrapper() {
  cc=$1; gl=$2; dst=$3; hdr=${4:-}; bsh=`command -v bash 2>/dev/null || command -v sh`
  ida=; [ -n "$hdr" ] && ida=" -idirafter $hdr"
  { printf '#!%s\n' "$bsh"
    printf 'for a in "$@"; do case "$a" in -shared) exec "%s"%s -B%s/lib "$@";; esac; done\n' "$cc" "$ida" "$gl"
    printf 'exec "%s" -static%s -B%s/lib "$@"\n' "$cc" "$ida" "$gl"
  } > "$dst"
  chmod 0555 "$dst"
}

# build_binutils_x86_64_native <cpath> <xgcc2> <xglibc> <xbu> <kh> <out>
#   NATIVE GNU Binutils 2.44 (--build=--host=--target=x86_64-pc-linux-gnu), built STATIC by the cross
#   gcc 14.3.0 ($xgcc2, an i686 binary emitting x86_64) vs the /td/store x86_64 glibc 2.41 static
#   archives. autoconf's --host=x86_64-pc-linux-gnu tool detection prefixes with $XTARGET-, so the
#   cross binutils ($xbu) on PATH satisfy AR/RANLIB/AS/LD for the build; CC = the cross gcc. The x86_64
#   glibc 2.41 headers #include <linux/…>, so the x86_64 kernel UAPI headers ($kh tarball) MUST be on the
#   include path beside them. Output: ELF 64-bit x86_64 plain-named as/ld/ar/... that run natively on x86_64.
build_binutils_x86_64_native() {
  cpath=$1; xgcc2=$2; xglibc=$3; xbu=$4; kh=$5; out=$6
  rm -rf "$out"; mkdir -p "$out"
  xzb=`_store_tool xz xz-`; test -n "$xzb" || { echo "no xz" >&2; return 1; }
  csh=`command -v bash 2>/dev/null || command -v sh`
  khd=`mktemp -d`/kh; mkdir -p "$khd"; tar -xzf "$kh" -C "$khd" || { echo "x86_64 kernel headers unpack failed" >&2; return 1; }
  CIP="$xglibc/include:$khd"
  wb=`mktemp -d`/wb; mkdir -p "$wb"   # -shared-aware static wrapper (handles binutils' ld libdep.la shared module)
  _mk_native_static_wrapper "$xgcc2/bin/$XTARGET-gcc" "$xglibc" "$wb/cc"
  tb=`mktemp -d`/tb; _xbin "$tb"
  src=`mktemp -d`/binutils; mkdir -p "$src"
  "$xzb" -dc "$BU244_TB" | tar -xf - -C "$src" --strip-components=1 || { echo "binutils-2.44 unpack failed" >&2; return 1; }
  ( cd "$src"; bp="$xbu/bin:$tb:$cpath"
    env PATH="$bp" CONFIG_SHELL="$csh" SHELL="$csh" CC="$wb/cc" CC_FOR_BUILD="$wb/cc" \
        C_INCLUDE_PATH="$CIP" \
        "$csh" ./configure --build=$XTARGET --host=$XTARGET --target=$XTARGET \
        --prefix=/td/store/binutils-2.44-x86_64-native \
        --disable-nls --disable-gold --disable-werror --enable-deterministic-archives \
        --disable-plugins --disable-gprofng --disable-multilib >cfg.log 2>&1 \
      || { echo "native x86_64 binutils configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_xnbu-cfg.log" 2>/dev/null||true; tail -25 cfg.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" SHELL="$csh" \
        C_INCLUDE_PATH="$CIP" make $X86_MAKE_J MAKEINFO=true >build.log 2>&1 \
      || { echo "native x86_64 binutils make failed" >&2; cp build.log "$ROOT/.td-build-cache/_xnbu-build.log" 2>/dev/null||true; tail -30 build.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" SHELL="$csh" \
        make MAKEINFO=true install prefix="$out" >inst.log 2>&1 \
      || { echo "native x86_64 binutils install failed" >&2; tail -20 inst.log >&2; return 1; } ) || return 1
  test -x "$out/bin/as" -a -x "$out/bin/ld" -a -x "$out/bin/readelf" || { echo "no native as/ld/readelf produced" >&2; return 1; }
  cls=`"$out/bin/readelf" -h "$out/bin/as" 2>/dev/null | grep -i 'class:' | grep -o 'ELF64'`
  test "$cls" = ELF64 || { echo "native binutils 'as' is not ELF64 x86_64 (got '$cls')" >&2; return 1; }
}

# build_gcc_x86_64_native <cpath> <xgcc2> <xglibc> <xbu> <xnbu> <kh_x86_64_tb> <out>
#   NATIVE GCC 14.3.0 (c,c++; --build=--host=--target=x86_64-pc-linux-gnu), built STATIC by the cross
#   gcc 14.3.0 ($xgcc2) vs the /td/store x86_64 glibc 2.41, with gmp-6.3.0/mpfr-4.2.1/mpc-1.3.1
#   in-tree. as/ld are the NATIVE x86_64 binutils ($xnbu) via --with-as/--with-ld. A combined build
#   sysroot ($out/sysroot/include = x86_64 glibc headers + x86_64 kernel UAPI) supplies the target
#   headers. CC is a single-token -static wrapper SCRIPT (gcc derives CC_FOR_BUILD from CC on a native
#   build and strips trailing flags from a plain CC_FOR_BUILD — a script survives the munging; same
#   reason as build_gcc_14). The produced gcc/cc1/g++ are ELF 64-bit x86_64 — a native compiler.
build_gcc_x86_64_native() {
  cpath=$1; xgcc2=$2; xglibc=$3; xbu=$4; xnbu=$5; kh=$6; out=$7
  rm -rf "$out"; mkdir -p "$out"
  xzb=`_store_tool xz xz-`; test -n "$xzb" || { echo "no xz" >&2; return 1; }
  csh=`command -v bash 2>/dev/null || command -v sh`
  "$xzb" -dc "$GCC14_TB" | tar -xf - -C "$out" --strip-components=1 || { echo "gcc-14.3.0 unpack failed" >&2; return 1; }
  "$xzb" -dc "$GMP63_TB" | tar -xf - -C "$out" || { echo "gmp unpack failed" >&2; return 1; }
  "$xzb" -dc "$MPFR421_TB" | tar -xf - -C "$out" || { echo "mpfr unpack failed" >&2; return 1; }
  tar -xzf "$MPC131_TB" -C "$out" || { echo "mpc unpack failed" >&2; return 1; }
  ( cd "$out" && ln -sf gmp-6.3.0 gmp && ln -sf mpfr-4.2.1 mpfr && ln -sf mpc-1.3.1 mpc ) || { echo "gmp/mpfr/mpc symlink failed" >&2; return 1; }
  # combined build sysroot: include/ = x86_64 glibc headers + x86_64 kernel UAPI (glibc headers #include
  # <linux/…>); lib/ = the x86_64 glibc 2.41 libs + crt, so the freshly-built target xgcc can LINK its
  # libgcc/libstdc++ conftests (--with-build-sysroot points the TARGET compiler here, not the wrapper).
  sysroot="$out/sysroot"; mkdir -p "$sysroot/include" "$sysroot/lib"
  cp -a "$xglibc/include/." "$sysroot/include/" || { echo "could not stage glibc headers into the sysroot" >&2; return 1; }
  tar -xzf "$kh" -C "$sysroot/include" || { echo "x86_64 kernel headers unpack failed" >&2; return 1; }
  cp -a "$xglibc/lib/." "$sysroot/lib/" || { echo "could not stage glibc libs into the sysroot" >&2; return 1; }
  # Relocate glibc's GNU ld scripts (libc.so, libm.so AND libm.a — the cross build only relocated *.so)
  # to BARE names: a fully-static host link pulls libm.a, whose GROUP script else points at the absolute
  # configure prefix /td/store/glibc-2.41-x86_64/lib (where the glibc is NOT) → "cannot find libm-2.41.a".
  for so in "$sysroot/lib/"*.so "$sysroot/lib/"*.a; do
    if head -c 80 "$so" 2>/dev/null | grep -qa 'GNU ld script'; then
      sed -i "s,/td/store/glibc-2.41-x86_64/lib/,,g" "$so" 2>/dev/null || true
    fi
  done
  wb="$out/wb"; mkdir -p "$wb"   # -shared-aware static wrappers; -B at the RELOCATED sysroot/lib; headers via -idirafter
  _mk_native_static_wrapper "$xgcc2/bin/$XTARGET-gcc" "$sysroot" "$wb/gcc" "$sysroot/include"
  _mk_native_static_wrapper "$xgcc2/bin/$XTARGET-g++" "$sysroot" "$wb/g++" "$sysroot/include"
  tb=`mktemp -d`/tb; _xbin "$tb"
  # the glibc + kernel headers come via the wrapper's -idirafter (NOT C_INCLUDE_PATH — that breaks the
  # libstdc++ <cstdlib> #include_next); CIP carries only the in-tree mpfr header dir for the host build.
  CIP="$out/mpfr/src"; LP="$sysroot/lib"
  ( cd "$out"
    for f in `grep -rl '^#! */bin/sh' . 2>/dev/null`; do sed -i "1s,^#! *[^ ]*/bin/sh,#!$csh," "$f" 2>/dev/null || true; done
    rm -rf bld; mkdir bld; cd bld
    env PATH="$xnbu/bin:$xbu/bin:$tb:$cpath" CONFIG_SHELL="$csh" \
        CC="$wb/gcc" CXX="$wb/g++" CPP="$wb/gcc -E" CC_FOR_BUILD="$wb/gcc" CXX_FOR_BUILD="$wb/g++" \
        C_INCLUDE_PATH="$CIP" CPLUS_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" LDFLAGS="-static" \
        "$csh" ../configure --prefix=/td/store/gcc-14.3.0-x86_64-native \
        --build=$XTARGET --host=$XTARGET --target=$XTARGET \
        --with-as="$xnbu/bin/as" --with-ld="$xnbu/bin/ld" \
        --with-build-sysroot="$sysroot" --with-native-system-header-dir=/include \
        --disable-bootstrap --disable-multilib --disable-shared --enable-static \
        --enable-languages=c,c++ --enable-threads=single --disable-libstdcxx-pch \
        --disable-libatomic --disable-libgomp --disable-libitm --disable-libsanitizer \
        --disable-libssp --disable-libvtv --disable-libquadmath --disable-lto --disable-plugin \
        --disable-libcc1 --disable-decimal-float --disable-werror >cfg.log 2>&1 \
      || { echo "native x86_64 gcc-14.3.0 configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_xngcc-cfg.log" 2>/dev/null||true; tail -25 cfg.log >&2; return 1; }
    env PATH="$xnbu/bin:$xbu/bin:$tb:$cpath" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" \
        C_INCLUDE_PATH="$CIP" CPLUS_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        make $X86_MAKE_J SHELL="$csh" CONFIG_SHELL="$csh" MAKEINFO=true "LDFLAGS=-static" "LDFLAGS_FOR_TARGET=-static" >build.log 2>&1 \
      || { echo "native x86_64 gcc-14.3.0 make failed" >&2; cp build.log "$ROOT/.td-build-cache/_xngcc-build.log" 2>/dev/null||true; tail -40 build.log >&2; return 1; }
    env PATH="$xnbu/bin:$xbu/bin:$tb:$cpath" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" \
        C_INCLUDE_PATH="$CIP" CPLUS_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        make SHELL="$csh" MAKEINFO=true install DESTDIR="$out/stage" >install.log 2>&1 \
      || { echo "native x86_64 gcc-14.3.0 install failed" >&2; tail -20 install.log >&2; return 1; } ) || return 1
  g="$out/stage/td/store/gcc-14.3.0-x86_64-native"
  test -x "$g/bin/gcc" -a -x "$g/bin/g++" || { echo "no native gcc/g++ produced" >&2; return 1; }
  cc1=`find "$g" -name cc1 | head -1`
  cls=`"$xnbu/bin/readelf" -h "$g/bin/gcc" 2>/dev/null | grep -i 'class:' | grep -o 'ELF64'`
  mch=`"$xnbu/bin/readelf" -h "$g/bin/gcc" 2>/dev/null | grep -i 'machine:' | grep -io 'x86-64'`
  test "$cls" = ELF64 -a -n "$mch" || { echo "native gcc is not ELF64 x86_64 (class='$cls' machine='$mch')" >&2; return 1; }
  test -n "$cc1" || { echo "native gcc produced no cc1" >&2; return 1; }
  # bundle plain as/ld into the native gcc's OWN tooldir so it resolves them relative to argv[0] in the
  # own-root (RELATIVE symlinks to the sibling native-binutils install path — the same self-contained
  # trick x86_64_bundle_tooldir uses for the cross gcc). MUST be set before the tree is interned.
  nbubase=`basename "$xnbu"`; mkdir -p "$g/$XTARGET/bin"
  for t in as ld ar nm ranlib strip objcopy objdump; do
    test -e "$xnbu/bin/$t" && ln -sf "../../../$nbubase/bin/$t" "$g/$XTARGET/bin/$t" || true
  done
}

# verify_x86_64_native_ownroot <cpath> <scratch> — the DURABLE own-root verify for rung X2. Interns the
# NATIVE x86_64 gcc + NATIVE x86_64 binutils + the x86_64 glibc 2.41 at /td/store, then RUNS the native
# gcc IN the store-ns own-root: it COMPILES a C and a C++ program from source and the results run → 42,
# /gnu/store ABSENT. The compiler doing the work is itself an ELF 64-bit x86_64 binary in /td/store.
# Requires the run_x86_64_cross / closure exports XGLIBC, plus $XNGCC (native gcc tree) and $XNBU
# (native binutils tree) from the caller. Legs: [no-guix] [native-arch] [content-addr] [behavioral]
# [self-host-compile] [structural].
verify_x86_64_native_ownroot() {
  cpath=$1; snwork=$2; store="$snwork/td-store-native"; sndb="$snwork/store-native.db"; mkdir -p "$store"
  test -n "${XNGCC:-}" -a -d "${XNGCC:-/nonexistent}" || { echo "native gcc tree (XNGCC) unset" >&2; return 1; }
  test -n "${XNBU:-}" -a -d "${XNBU:-/nonexistent}" || { echo "native binutils tree (XNBU) unset" >&2; return 1; }
  test -n "${XGLIBC:-}" -a -d "${XGLIBC:-/nonexistent}" || { echo "x86_64 glibc tree (XGLIBC) unset" >&2; return 1; }
  ngcc=`"$XNBU/bin/readelf" -h "$XNGCC/bin/gcc" 2>/dev/null`
  echo "$ngcc" | grep -i 'class:' | grep -q 'ELF64' || { echo "native gcc not ELF64" >&2; return 1; }
  echo "$ngcc" | grep -i 'machine:' | grep -qi 'x86-64' || { echo "native gcc machine is not x86-64" >&2; return 1; }
  echo "   [native-arch] the native gcc/binutils ARE ELF 64-bit x86_64 binaries (not the i686 cross gcc)"
  ncc1=`find "$XNGCC" -name cc1 | head -1`
  for b in "$XNGCC/bin/gcc" "$ncc1" "$XNBU/bin/as" "$XNBU/bin/ld" "$XGLIBC/lib/libc.so.6"; do
    test -n "$b" -a -e "$b" || { echo "native output missing ($b)" >&2; return 1; }
    if grep -q -a '/gnu/store' "$b"; then echo "$b contains /gnu/store bytes" >&2; return 1; fi
  done
  echo "   [no-guix] the native gcc/cc1 + native as/ld + the x86_64 libc.so.6 carry no /gnu/store bytes"
  # intern the closure: native binutils, native gcc (tooldir as/ld symlink to the native binutils base),
  # and the x86_64 glibc 2.41 — all as siblings so the relative tooldir symlinks resolve under /td/store.
  # Intern the native binutils FIRST to learn its content-addressed basename, then re-point the native
  # gcc's tooldir as/ld symlinks at THAT basename BEFORE interning the gcc (so the interned, content-
  # addressed gcc tree is internally consistent — not patched after the fact).
  NBP=`"$TB" store-add-recursive "\`basename "$XNBU"\`" "$XNBU" "$store" "$sndb"` || { echo "store-add native binutils failed" >&2; return 1; }
  nbrel=`basename "$NBP"`
  # Re-point the native gcc's tooldir as/ld/... at the INTERNED native-binutils basename ($nbrel) before
  # interning the gcc. Key on the SOURCE ($XNBU/bin/$t), NOT the dest: the build-time symlinks point at the
  # mktemp basename and are DANGLING here, so `test -e <dest>` (which FOLLOWS the link) would be false and
  # skip every tool — leaving the interned tooldir pointing at a non-existent /td/store/native-binutils.
  mkdir -p "$XNGCC/$XTARGET/bin"
  for t in as ld ar nm ranlib strip objcopy objdump; do
    test -e "$XNBU/bin/$t" && ln -sf "../../../$nbrel/bin/$t" "$XNGCC/$XTARGET/bin/$t" || true
  done
  NGP=`"$TB" store-add-recursive "\`basename "$XNGCC"\`" "$XNGCC" "$store" "$sndb"` || { echo "store-add native gcc failed" >&2; return 1; }
  GLP=`"$TB" store-add-recursive glibc-2.41-x86_64 "$XGLIBC" "$store" "$sndb"` || { echo "store-add x86_64 glibc failed" >&2; return 1; }
  case "$NGP" in /td/store/*-gcc-14.3.0-x86_64-native) ;; *) echo "native gcc not content-addressed: $NGP" >&2; return 1 ;; esac
  echo "   [content-addr] interned the native gcc ($NGP), native binutils, and the x86_64 glibc in /td/store"
  ngrel=`basename "$NGP"`; glrel=`basename "$GLP"`
  chmod -R u+w "$store"
  # [self-contained] DURABLE (no guix oracle): the interned native gcc carries as/ld in its OWN tooldir
  # ($XTARGET/bin) resolving (as a SIBLING /td/store path) to the interned native binutils — so the native
  # gcc finds its assembler/linker relative to argv[0], not only via PATH. A regression that drops/breaks
  # the re-point above reds HERE (the symlink would be dangling), instead of being masked by the probe's
  # PATH fallback. Mirrors the cross gcc's [self-contained] leg (x86_64_bundle_tooldir).
  for t in as ld; do
    test -e "$store/$ngrel/$XTARGET/bin/$t" || { echo "native gcc tooldir '$t' is dangling (not the interned /td/store/$nbrel) — not self-contained" >&2; return 1; }
    case `readlink "$store/$ngrel/$XTARGET/bin/$t"` in */"$nbrel"/bin/"$t") ;; *) echo "native gcc tooldir '$t' does not point at the interned native binutils $nbrel" >&2; return 1 ;; esac
  done
  echo "   [self-contained] the interned native gcc bundles as/ld in its own tooldir ($XTARGET/bin) → the interned /td/store native binutils, resolved relative to argv[0]"
  bashlock=`grep -- '-bash-' tests/hello-no-guix.lock | grep -v static | sed 's/^[^ ]* //' | head -1`
  bs=`"$TB" store-closure /var/guix/db/db.sqlite "$bashlock" | grep -- '-bash-static-' | head -1`
  bbase=`basename "$bs"`; cp -a "$bs" "$store/$bbase"; chmod -R u+w "$store"
  # the probe is a FILE in the (ro) store; it compiles into the writable tmpfs /tmp inside the own-root.
  # the probe runs the NATIVE gcc IN the own-root. It uses ONLY bash builtins (cd/printf/case/[) + the
  # store's own binaries (gcc/g++/readelf) — the own-root has NO coreutils (no mkdir/grep/sed). glibc
  # headers come via -idirafter (NOT C_INCLUDE_PATH — the libstdc++ <cstdlib> #include_next), and
  # -B + -rpath/interp point at the interned /td/store glibc so the produced binaries are DYNAMIC
  # (libc.so.6, interp = the /td/store ld) and run via the bound glibc. /tmp is store-ns's writable tmpfs.
  cat > "$store/nativeprobe.sh" <<PROBE
export PATH=/td/store/$ngrel/bin:/td/store/$nbrel/bin
H="-idirafter /td/store/$glrel/include"
B="-B/td/store/$glrel/lib"
LD="-Wl,--dynamic-linker,/td/store/$glrel/lib/ld-linux-x86-64.so.2,-rpath,/td/store/$glrel/lib"
cd /tmp || exit 1
printf 'int main(){return 42;}\n' > c.c
printf '#include <vector>\n#include <cstdlib>\nint main(){std::vector<int> v; for(int i=0;i<43;i++) v.push_back(i); return v[42];}\n' > cpp.cc
gcc \$B \$LD -o c c.c || { echo "NATIVE-CC-FAIL"; exit 1; }
g++ -O2 \$H -static-libgcc -static-libstdc++ \$B \$LD -o cpp cpp.cc || { echo "NATIVE-CXX-FAIL"; exit 1; }
hdr=\$(/td/store/$nbrel/bin/readelf -h c)
case "\$hdr" in *ELF64*) echo CCLASS=ELF64 ;; esac
case "\$hdr" in *X86-64*|*x86-64*) echo CMACH=x86-64 ;; esac
itp=\$(/td/store/$nbrel/bin/readelf -l c)
case "\$itp" in *"/td/store/$glrel/lib/ld-linux-x86-64.so.2"*) echo CINTERP=OK ;; esac
./c; echo "CRC=\$?"
./cpp; echo "CPPRC=\$?"
[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
PROBE
  out=`"$TB" store-ns "$store" -- "/td/store/$bbase/bin/bash" /td/store/nativeprobe.sh 2>&1` \
    || { printf '%s\n' "$out" | sed 's/^/     /' >&2; echo "store-ns native-gcc probe exited nonzero" >&2; return 1; }
  printf '%s\n' "$out" | sed 's/^/     /' >&2
  echo "$out" | grep -q '^CCLASS=ELF64$' || { echo "the native gcc did not emit an ELF64 program in the own-root" >&2; return 1; }
  echo "$out" | grep -q '^CMACH=x86-64$' || { echo "the native-gcc-compiled program is not x86-64" >&2; return 1; }
  echo "$out" | grep -q '^CINTERP=OK$' || { echo "the native-gcc-compiled program's interp is not the /td/store x86_64 ld" >&2; return 1; }
  echo "$out" | grep -q '^CRC=42$'   || { echo "the native-gcc-compiled C program did not return 42 in the own-root" >&2; return 1; }
  echo "$out" | grep -q '^CPPRC=42$' || { echo "the native-gcc-compiled C++ program did not return 42 in the own-root" >&2; return 1; }
  echo "   [self-host-compile] the NATIVE x86_64 gcc RAN in the own-root and compiled a DYNAMIC ELF64 x86-64 C AND C++ program (interp = the /td/store x86_64 ld) from source → both run → 42"
  echo "$out" | grep -q '^GNU-ABSENT$' || { echo "/gnu/store is PRESENT in the own-root" >&2; return 1; }
  echo "   [structural] inside td's own root /td/store IS the store AND /gnu/store is ABSENT"
}
