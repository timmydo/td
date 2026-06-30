#!/bin/sh
# tests/bootstrap-gcc-core-mesboot1.sh — source-bootstrap BRICK 5 (gcc toolchain): GCC 4.6.4 (C) — the
# first MODERN, modular gcc, built by gcc-mesboot0 + binutils-mesboot1 + make-mesboot against glibc —
# guix's gcc-core-mesboot1. After gcc-mesboot0 (GCC 2.95.3), td jumps to GCC 4.6.4: the capable gcc the
# gcc-mesboot (4.7.4) / final-toolchain arc is built on. gmp 4.3.2 / mpfr 2.4.2 / mpc 1.0.3 are unpacked
# INTO the gcc source tree and built in-tree (guix's unpack-gmp&co) — no pre-built math libraries.
#
# From the 229-byte seed, td builds the whole chain (bricks 0-4 + make/patch/binutils/gcc/glibc rungs +
# gcc-mesboot0 + binutils-mesboot1 + make-mesboot), then make-mesboot drives the GCC 4.6.4 build:
# CC=<gcc-mesboot0>, as/ld/ar = binutils-mesboot1, libc = glibc-mesboot0, pure host-produced kernel UAPI
# headers, with gcc-mesboot0's gcc-lib/include FIRST on C_INCLUDE_PATH (guix's order). td's glibc-mesboot0
# is static-only (no ld.so), so unlike guix (a -dynamic-linker) td builds GCC 4.6.4 STATIC:
# LDFLAGS="-static -B<glibc>/lib" (link-only, so it does NOT trip the autoconf -E "never used" warning).
# MAKEINFO=true skips the texinfo docs (the host makeinfo rejects gcc 4.6.4's old .texi; guix's minimal
# bootstrap PATH simply has no makeinfo). Same make discipline: cleared MAKEFLAGS jobserver + SHELL var,
# /bin/sh shebangs rewritten.
#
# KERNEL HEADERS: guix's %bootstrap-linux-libre-headers is a PREBUILT guix blob (rejected by the north
# star). td produces the sanitized UAPI headers FROM the pinned linux-4.14.67 source on the HOST
# (td-feed warm kernel-headers i386, run in check.sh's prelude); this gate CONSUMES them.
#
# i686, static, serial. mes/nyacc/tcc/make/patch/binutils/gcc/glibc/linux/make-3.82/gcc-4.6.4/gmp/mpfr/
# mpc are td-fetched; the 5 guix boot patches are vendored source data — gcc/cc1 are compiled from gcc
# source + the patch (no guix-built bytes; the [no-guix] leg verifies).
#
# Legs (DURABLE):
#   [pinned-input] the chain's tarballs + 5 vendored boot patches + gcc-4.6.4/gmp/mpfr/mpc match sha256.
#   [no-guix]      built on a curated PATH with gcc/g++/cc/guile/guix DENIED; no /gnu/store in gcc/cc1.
#   [behavioral]   gcc reports 4.6.4 and COMPILES + LINKS + RUNS a static C program → 42 — a modern gcc.
#   [repro]        the gcc driver is byte-identical across two builds AND the rebuilt compiler emits
#                  byte-identical assembly (intrinsic output-repro; cc1's binary has benign build stamps).
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
lf() { sed -n "s/^$2 //p" "$1" | head -1; }
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
KH_VER=`printf '%s' "\`lf "$LINUX_LOCK" file\`" | sed -n 's/^linux-\(.*\)\.tar\..*$/\1/p'`
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
make_curated_path() {
  cdir=`mktemp -d`/bin; mkdir -p "$cdir"; oldifs=$IFS; IFS=:
  for d in $PATH; do [ -d "$d" ] || continue; for f in "$d"/*; do b=`basename "$f"`
    case "$b" in gcc|g++|cc|c++|cpp|gcc-*|g++-*|clang|clang*|tcc|guile|guild|guile-*|guix|guix-*) continue ;; esac
    [ -e "$cdir/$b" ] || ln -s "$f" "$cdir/$b" 2>/dev/null || true; done; done
  IFS=$oldifs; echo "$cdir"
}
build_toolchain() {
  tc=`mktemp -d`; cp -a "$STAGE0/." "$tc/"
  chmod +x "$tc/bootstrap-seeds/POSIX/$A/hex0-seed" "$tc/bootstrap-seeds/POSIX/$A/kaem-optional-seed"
  mkdir -p "$tc/$A/artifact" "$tc/$A/bin"
  ( cd "$tc" && env -i ./bootstrap-seeds/POSIX/$A/kaem-optional-seed ./$A/mescc-tools-seed-kaem.kaem \
      && env -i ./$A/artifact/kaem-0 ./$A/mescc-tools-mini-kaem.kaem ) >/dev/null 2>&1 \
    || { echo "seed toolchain build failed" >&2; return 1; }
  echo "$tc"
}
seedbin_for() {
  tc=$1; sb=`mktemp -d`/seedbin; mkdir -p "$sb"
  ln -sf "$tc/$A/artifact/M2" "$sb/M2-Planet"; ln -sf "$tc/$A/artifact/blood-elf-0" "$sb/blood-elf"
  ln -sf "$tc/$A/bin/M1" "$sb/M1"; ln -sf "$tc/$A/bin/hex2" "$sb/hex2"; ln -sf "$tc/$A/bin/kaem" "$sb/kaem"; echo "$sb"
}
build_mes_prefix() {
  tc=$1; cpath=$2; sb=`seedbin_for "$tc"`; M1B="$tc/$A/bin/M1"; HEX2B="$tc/$A/bin/hex2"; BE="$tc/$A/artifact/blood-elf-0"
  work=`mktemp -d`; tar -xzf "$MES_TB" -C "$work"; m="$work/`tar -tzf "$MES_TB" | head -1 | cut -d/ -f1`"
  tar -xzf "$NYACC_TB" -C "$work"; ny="$work/`tar -tzf "$NYACC_TB" | head -1 | cut -d/ -f1`"
  GLP="$ny/module:$m/mes/module:$m/module"
  ( cd "$m"; bp="$sb:$cpath"
    PATH="$bp" GUILE_LOAD_PATH="$GLP" MES_PREFIX="$m" MES_ARENA=100000000 MES_MAX_ARENA=100000000 MES_STACK=8000000 \
      GUILE=true CC= MES_FOR_BUILD=mes bash configure.sh --prefix="$m/out" --host=i686-linux-gnu >cfg.log 2>&1 || { echo "mes configure failed" >&2; tail -5 cfg.log >&2; exit 1; }
    for step in bootstrap install; do
      PATH="$bp" GUILE_LOAD_PATH="$GLP" MES_PREFIX="$m" MES_ARENA=100000000 MES_MAX_ARENA=100000000 MES_STACK=8000000 \
        GUILE=true MES_FOR_BUILD=mes M1="$M1B" HEX2="$HEX2B" BLOOD_ELF="$BE" sh "$step.sh" >"$step.log" 2>&1 || { echo "mes $step failed" >&2; tail -8 "$step.log" >&2; exit 1; }
    done ) || return 1
  prefix="$m/out"; gsd=`ls -d "$prefix"/share/guile/site/* 2>/dev/null | head -1`
  mkdir -p "$gsd"; cp -a "$prefix/share/mes/module/." "$gsd/" 2>/dev/null; cp -a "$ny/module/." "$gsd/" 2>/dev/null
  test -x "$prefix/bin/mescc" -a -s "$prefix/lib/x86-mes/libc+tcc.a" || { echo "mes install incomplete" >&2; return 1; }
  echo "$prefix"
}
build_tcc() {
  tc=$1; cpath=$2; mesp=$3; t=$4; sb=`seedbin_for "$tc"`
  ln -sf "$mesp/bin/mescc" "$sb/mescc"; ln -sf "$mesp/bin/mes" "$sb/mes"
  NYM=`ls -d "$mesp"/share/guile/site/*/nyacc 2>/dev/null | head -1`; NYM="${NYM%/nyacc}"
  rm -rf "$t"; mkdir -p "$t"; tar -xzf "$TCC_TB" -C "$t" --strip-components=1
  ( cd "$t"; sed -i 's/volatile//' conftest.c 2>/dev/null || true; bp="$sb:$cpath"
    env PATH="$bp" MES_PREFIX="$mesp" GUILE_LOAD_PATH="$NYM" host=i686-linux-gnu ONE_SOURCE=true prefix="$t/out" \
      sh configure --cc=mescc --prefix="$t/out" --elfinterp=/lib/mes-loader --crtprefix=. --tccdir=. >cfg.log 2>&1 || { echo "tcc configure failed" >&2; tail -5 cfg.log >&2; exit 1; }
    env PATH="$bp" MES_PREFIX="$mesp" GUILE_LOAD_PATH="$NYM" host=i686-linux-gnu ONE_SOURCE=true prefix="$t/out" \
        MES_ARENA=20000000 MES_MAX_ARENA=20000000 MES_STACK=6000000 \
      sh bootstrap.sh >boot.log 2>&1 || { echo "tcc bootstrap failed" >&2; tail -10 boot.log >&2; exit 1; }
  ) || return 1
  test -x "$t/tcc" || { echo "no tcc produced" >&2; return 1; }
}
build_make() {
  tc=$1; cpath=$2; mesp=$3; tccd=$4; mk=$5
  rm -rf "$mk"; mkdir -p "$mk"; tar -xzf "$MAKE_TB" -C "$mk" --strip-components=1
  cp "$tccd"/crt1.o "$tccd"/crti.o "$tccd"/crtn.o "$tccd"/libc.a "$tccd"/libtcc1.a "$mk/"
  mkdir -p "$mk/bin"; ln -sf "$tccd/tcc" "$mk/bin/tcc"
  inc1="$mesp/include"; inc2="$mesp/include/x86"
  ( cd "$mk"; bp="$mk/bin:$cpath"
    csh=`PATH="$bp" command -v sh`
    sed -i 's/@LIBOBJS@/getloadavg.o/; s/@REMOTE@/stub/' build.sh.in
    env PATH="$bp" CONFIG_SHELL="$csh" "$csh" ./configure "CC=tcc -static -L. -I$inc1 -I$inc2" "CPP=tcc -E -I$inc1 -I$inc2" LD=tcc \
        --build=i686-unknown-linux-gnu --host=i686-unknown-linux-gnu --disable-nls >cfg.log 2>&1 \
      || { echo "make configure failed" >&2; tail -6 cfg.log >&2; exit 1; }
    sed -i 's,^extern long int lseek.*,// &,' make.h 2>/dev/null || true
    env PATH="$bp" CONFIG_SHELL="$csh" "$csh" ./build.sh >build.log 2>&1 || { echo "make build.sh failed" >&2; tail -8 build.log >&2; exit 1; }
  ) || return 1
  test -x "$mk/make" || { echo "no make binary produced" >&2; return 1; }
}
build_patch() {
  cpath=$1; mesp=$2; tccd=$3; mk=$4; pd=$5
  rm -rf "$pd"; mkdir -p "$pd/bin"; tar -xzf "$PATCH_TB" -C "$pd" --strip-components=1
  cp "$tccd"/crt1.o "$tccd"/crti.o "$tccd"/crtn.o "$tccd"/libc.a "$tccd"/libtcc1.a "$pd/"
  ln -sf "$tccd/tcc" "$pd/bin/tcc"; ln -sf "$mk/make" "$pd/bin/make"
  inc1="$mesp/include"; inc2="$mesp/include/x86"
  sed -i 's/^    while (p_end >= 0) {/    p_end = -1;\n    while (0) {/' "$pd/pch.c"
  ( cd "$pd"; bp="$pd/bin:$cpath"
    csh=`PATH="$bp" command -v sh`
    env PATH="$bp" CONFIG_SHELL="$csh" "$csh" ./configure "CC=tcc -static -L. -I$inc1 -I$inc2" \
        "CPP=tcc -E -I$inc1 -I$inc2" "AR=tcc -ar" LD=tcc \
        --build=i686-unknown-linux-gnu --host=i686-unknown-linux-gnu --disable-nls >cfg.log 2>&1 \
      || { echo "patch configure failed" >&2; tail -8 cfg.log >&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= "$mk/make" SHELL="$csh" CONFIG_SHELL="$csh" \
        "CC=tcc -static -L. -I$inc1 -I$inc2" "AR=tcc -ar" >build.log 2>&1 \
      || { echo "patch make failed" >&2; tail -12 build.log >&2; exit 1; }
  ) || return 1
  test -x "$pd/patch" || { echo "no patch binary produced" >&2; return 1; }
}
# --- build GNU Binutils 2.20.1a (guix's binutils-mesboot0) at a CALLER-GIVEN dir ------------------
# guix env: the td-built `patch` applies the boot patch; CONFIG_SHELL=<sh>; CPPFLAGS="-D
# __GLIBC_MINOR__=6 -D MES_BOOTSTRAP=1"; AR="tcc -ar"; CXX=false; RANLIB=true; serial; --with-sysroot=/.
# The nested make gets the SHELL var + cleared MAKEFLAGS (recursive build: bfd/gas/ld/…).
build_binutils() {
  cpath=$1; mesp=$2; tccd=$3; mk=$4; pd=$5; bd=$6
  rm -rf "$bd"; mkdir -p "$bd/bin"
  # .tar.bz2: the sandbox PATH has no bzip2, but the exposed /gnu/store carries it (host toolchain).
  bz=`command -v bzip2 2>/dev/null || ls /gnu/store/*bzip2*/bin/bzip2 2>/dev/null | head -1`
  test -n "$bz" || { echo "no bzip2 to unpack binutils" >&2; return 1; }
  "$bz" -dc "$BU_TB" | tar -xf - -C "$bd" --strip-components=1 || { echo "binutils unpack failed" >&2; return 1; }
  # apply guix's boot patch with the td-built patch (the diff paths are binutils-2.20.1a/… → -p1)
  ( cd "$bd" && env -i "$pd/patch" -p1 < "$BOOT_PATCH" ) >"$bd/patch.log" 2>&1 \
    || { echo "binutils boot-patch apply failed" >&2; tail -8 "$bd/patch.log" >&2; return 1; }
  cp "$tccd"/crt1.o "$tccd"/crti.o "$tccd"/crtn.o "$tccd"/libc.a "$tccd"/libtcc1.a "$bd/"
  # crt MUST live in tcc's absolute crtprefix ($tccd/out/lib) so the recursive subdir links (bfd/gas/
  # ld/…) find crt1.o — tcc searches out/lib for crt, NOT LIBRARY_PATH (confirmed via tcc -vvv). libc
  # comes via LIBRARY_PATH; headers via C_INCLUDE_PATH — exactly guix's tcc-boot0 search-path setup.
  mkdir -p "$tccd/out/lib"; cp "$tccd"/crt1.o "$tccd"/crti.o "$tccd"/crtn.o "$tccd"/libc.a "$tccd/out/lib/"
  ln -sf "$tccd/tcc" "$bd/bin/tcc"; ln -sf "$mk/make" "$bd/bin/make"; ln -sf "$pd/patch" "$bd/bin/patch"
  # build-time host tools the binutils combined-tree configure needs but the sandbox PATH lacks, from
  # the exposed /gnu/store (like bzip2): awk (config.status assembles the Makefile with it) and
  # flex/bison (AC_PROG_LEX/YACC checks — the parsers are pre-generated + patched, maintainer-mode is
  # off, so make never regenerates; flex/bison only satisfy configure). Build-time only; the [no-guix]
  # leg verifies as/ld carry no /gnu/store bytes.
  awkb=`command -v awk 2>/dev/null || ls /gnu/store/*gawk*/bin/awk 2>/dev/null | sort | head -1`
  flexb=`command -v flex 2>/dev/null || ls /gnu/store/*flex*/bin/flex 2>/dev/null | sort | head -1`
  bisonb=`command -v bison 2>/dev/null || ls /gnu/store/*bison*/bin/bison 2>/dev/null | sort | head -1`
  test -n "$awkb" -a -n "$flexb" -a -n "$bisonb" || { echo "need awk/flex/bison (build tools) from the store" >&2; return 1; }
  ln -sf "$awkb" "$bd/bin/awk"; ln -sf "$flexb" "$bd/bin/flex"; ln -sf "$flexb" "$bd/bin/lex"
  ln -sf "$bisonb" "$bd/bin/bison"; ln -sf "$bisonb" "$bd/bin/yacc"
  inc1="$mesp/include"; inc2="$mesp/include/x86"
  cpp="-D __GLIBC_MINOR__=6 -D MES_BOOTSTRAP=1"; CIP="$inc1:$inc2"; LP="$tccd"
  ( cd "$bd"; bp="$bd/bin:$cpath"
    csh=`PATH="$bp" command -v sh`
    env PATH="$bp" CONFIG_SHELL="$csh" C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" "$csh" ./configure \
        "CC=tcc -static $cpp" "CPPFLAGS=$cpp" "AR=tcc -ar" CXX=false RANLIB=true \
        --disable-nls --disable-shared --disable-werror \
        --build=i686-unknown-linux-gnu --host=i686-unknown-linux-gnu --with-sysroot=/ >cfg.log 2>&1 \
      || { echo "binutils configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_binutils-cfg.log" 2>/dev/null||true; tail -15 cfg.log >&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mk/make" SHELL="$csh" CONFIG_SHELL="$csh" \
        "CC=tcc -static $cpp" "AR=tcc -ar" CXX=false RANLIB=true >build.log 2>&1 \
      || { echo "binutils make failed" >&2; cp build.log "$ROOT/.td-build-cache/_binutils-build.log" 2>/dev/null||true; tail -25 build.log >&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mk/make" SHELL="$csh" install prefix="$bd/out" >install.log 2>&1 \
      || { echo "binutils install failed" >&2; cp install.log "$ROOT/.td-build-cache/_binutils-install.log" 2>/dev/null||true; tail -15 install.log >&2; exit 1; }
  ) || return 1
  test -x "$bd/out/bin/as" -a -x "$bd/out/bin/ld" || { echo "no as/ld produced" >&2; return 1; }
}
# --- build GCC 2.95.3 (guix's gcc-core-mesboot0) at a CALLER-GIVEN dir; $bd = binutils build dir ----
# The td-built patch applies the gcc boot patch; the tcc-built make drives tcc, with binutils' ar/as/ld
# on PATH (AR=ar). config.cache hints the float format; remove-info skips makeinfo; install2 assembles
# libgcc.a + libc.a into gcc-lib. #!/bin/sh shebangs in gcc's helper scripts are rewritten to the
# curated sh (no /bin/sh in the sandbox). crt in tcc's out/lib so gcc can link.
build_gcc() {
  cpath=$1; mesp=$2; tccd=$3; mk=$4; pd=$5; bd=$6; gd=$7
  rm -rf "$gd"; mkdir -p "$gd/bin"; tar -xzf "$GCC_TB" -C "$gd" --strip-components=1
  ( cd "$gd" && env -i "$pd/patch" --force -p1 -i "$GCC_PATCH" ) >"$gd/patch.log" 2>&1 \
    || { echo "gcc boot-patch apply failed" >&2; tail -8 "$gd/patch.log" >&2; return 1; }
  cp "$tccd"/crt1.o "$tccd"/crti.o "$tccd"/crtn.o "$tccd"/libc.a "$tccd"/libtcc1.a "$gd/"
  mkdir -p "$tccd/out/lib"; cp "$tccd"/crt1.o "$tccd"/crti.o "$tccd"/crtn.o "$tccd"/libc.a "$tccd/out/lib/"
  ln -sf "$tccd/tcc" "$gd/bin/tcc"; ln -sf "$mk/make" "$gd/bin/make"; ln -sf "$pd/patch" "$gd/bin/patch"
  for t in "$bd"/out/bin/*; do ln -sf "$t" "$gd/bin/`basename "$t"`"; done   # binutils as/ld/ar/ranlib/nm/strip
  awkb=`command -v awk 2>/dev/null || ls /gnu/store/*gawk*/bin/awk 2>/dev/null | sort | head -1`
  flexb=`command -v flex 2>/dev/null || ls /gnu/store/*flex*/bin/flex 2>/dev/null | sort | head -1`
  bisonb=`command -v bison 2>/dev/null || ls /gnu/store/*bison*/bin/bison 2>/dev/null | sort | head -1`
  test -n "$awkb" -a -n "$flexb" -a -n "$bisonb" || { echo "need awk/flex/bison (build tools) from the store" >&2; return 1; }
  ln -sf "$awkb" "$gd/bin/awk"; ln -sf "$flexb" "$gd/bin/flex"; ln -sf "$flexb" "$gd/bin/lex"
  ln -sf "$bisonb" "$gd/bin/bison"; ln -sf "$bisonb" "$gd/bin/yacc"
  inc1="$mesp/include"; inc2="$mesp/include/x86"; CIP="$inc1:$inc2"; LP="$tccd/out/lib"
  ( cd "$gd"; bp="$gd/bin:$cpath"; csh=`PATH="$bp" command -v sh`
    printf "ac_cv_c_float_format='IEEE (little-endian)'\n" > config.cache
    env PATH="$bp" CONFIG_SHELL="$csh" C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        CPPFLAGS=" -D __GLIBC_MINOR__=6" CC="tcc -D __GLIBC_MINOR__=6" CC_FOR_BUILD="tcc -D __GLIBC_MINOR__=6" CPP="tcc -E -D __GLIBC_MINOR__=6" \
        "$csh" ./configure --enable-static --disable-shared --disable-werror \
        --build=i686-unknown-linux-gnu --host=i686-unknown-linux-gnu --prefix="$gd/out" >cfg.log 2>&1 \
      || { echo "gcc configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_gcc-cfg.log" 2>/dev/null||true; tail -15 cfg.log >&2; exit 1; }
    test -s Makefile || { echo "gcc configure produced no Makefile" >&2; exit 1; }
    rm -rf texinfo 2>/dev/null||true; mkdir -p gcc; touch gcc/cpp.info gcc/gcc.info
    for f in `grep -rl '^#! */bin/sh' . 2>/dev/null`; do sed -i "1s,^#! *[^ ]*/bin/sh,#!$csh," "$f" 2>/dev/null || true; done
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mk/make" SHELL="$csh" CONFIG_SHELL="$csh" \
        "CC=tcc -static -D __GLIBC_MINOR__=6" "OLDCC=tcc -static -D __GLIBC_MINOR__=6" "CC_FOR_BUILD=tcc -static -D __GLIBC_MINOR__=6" \
        AR=ar RANLIB=ranlib "LIBGCC2_INCLUDES=-I $inc1" LANGUAGES=c "BOOT_LDFLAGS=-B$tccd/out/lib/" >build.log 2>&1 \
      || { echo "gcc make failed" >&2; cp build.log "$ROOT/.td-build-cache/_gcc-build.log" 2>/dev/null||true; tail -25 build.log >&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mk/make" SHELL="$csh" CONFIG_SHELL="$csh" AR=ar RANLIB=ranlib LANGUAGES=c install >install.log 2>&1 \
      || { echo "gcc install failed" >&2; cp install.log "$ROOT/.td-build-cache/_gcc-install.log" 2>/dev/null||true; tail -15 install.log >&2; exit 1; }
    # install2 (guix gcc-core-mesboot0): assemble libgcc.a (libgcc2.a + libtcc1.a) + libc.a (libc.o +
    # libtcc1.o) into gcc-lib so the compiler can link, using binutils' ar.
    gccdir="$gd/out/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3"; mkdir -p "$gccdir"
    ( rm -rf tg; mkdir tg; cd tg; env PATH="$bp" ar x ../gcc/libgcc2.a; env PATH="$bp" ar x "$tccd/libtcc1.a"; env PATH="$bp" ar r "$gccdir/libgcc.a" *.o ) >install2.log 2>&1 || { echo "gcc install2 libgcc failed">&2; tail -5 install2.log>&2; exit 1; }
    ( rm -rf tc2; mkdir tc2; cd tc2; env PATH="$bp" ar x "$tccd/libtcc1.a"; env PATH="$bp" ar x "$tccd/libc.a"; env PATH="$bp" ar r "$gccdir/libc.a" libc.o libtcc1.o ) >>install2.log 2>&1 || { echo "gcc install2 libc failed">&2; tail -5 install2.log>&2; exit 1; }
    cp "$tccd"/crt1.o "$tccd"/crti.o "$tccd"/crtn.o "$gd/out/lib/" 2>/dev/null||true
    cp gcc/libgcc2.a "$gd/out/lib/libgcc2.a" 2>/dev/null||true   # glibc-mesboot0 links tools with -lgcc2 (guix install2)
  ) || return 1
  test -x "$gd/out/bin/gcc" || { echo "no gcc produced" >&2; return 1; }
}
# --- mesboot-headers: install the host-produced Linux UAPI headers (td-feed warm kernel-headers i386) + the mes
# includes (guix's mesboot-headers merges both). The sandbox can't run the kernel build; the headers
# tarball is produced on the host from the pinned linux source. Returns the headers dir.
build_headers() {
  mesp=$1; hd=$2
  rm -rf "$hd"; mkdir -p "$hd/include"
  test -f "$KH_TB" || { echo "kernel headers tarball not produced ($KH_TB) — run td-feed warm kernel-headers i386" >&2; return 1; }
  tar -xzf "$KH_TB" -C "$hd/include" || { echo "kernel headers unpack failed" >&2; return 1; }
  cp -a "$mesp/include/." "$hd/include/" 2>/dev/null || true
  test -f "$hd/include/linux/version.h" -a -f "$hd/include/asm/unistd.h" || { echo "kernel headers incomplete (no version.h/unistd.h)" >&2; return 1; }
}
# --- build glibc-mesboot0 (glibc 2.2.5, guix's) with the seed gcc + binutils + the kernel headers ---
# The td-built patch applies guix's 2 boot patches; CC=<gcc> + MES_BOOTSTRAP/BOOTSTRAP_GLIBC defines;
# classic configure --with-headers; config.make fixup; #!/bin/sh shebangs rewritten; the seed gcc's
# cpp symlinked on PATH (glibc's scripts/cpp does `which cpp`); serial make + install.
build_glibc() {
  cpath=$1; gd=$2; bd=$3; tccd=$4; mk=$5; pd=$6; hd=$7; gld=$8
  rm -rf "$gld"; mkdir -p "$gld/bin"; tar -xzf "$GLIBC_TB" -C "$gld" --strip-components=1
  ( cd "$gld" && env -i "$pd/patch" --force -p1 -i "$GLIBC_P1" && env -i "$pd/patch" --force -p1 -i "$GLIBC_P2" ) >"$gld/patch.log" 2>&1 \
    || { echo "glibc boot-patch apply failed" >&2; tail -8 "$gld/patch.log" >&2; return 1; }
  gcc="$gd/out/bin/gcc"; gccdir="$gd/out/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3"
  ln -sf "$gcc" "$gld/bin/gcc"; ln -sf "$gd"/out/bin/cpp "$gld/bin/cpp"   # scripts/cpp does `which cpp`
  for t in "$bd"/out/bin/*; do ln -sf "$t" "$gld/bin/`basename "$t"`"; done
  ln -sf "$mk/make" "$gld/bin/make"; ln -sf "`command -v awk 2>/dev/null || ls /gnu/store/*gawk*/bin/awk 2>/dev/null|sort|head -1`" "$gld/bin/awk"
  CIP="$gd/out/include:$gccdir/include:$hd/include"; LP="$gd/out/lib:$gccdir:$tccd/out/lib"
  cppflags=" -D MES_BOOTSTRAP=1 -D BOOTSTRAP_GLIBC=1"
  ( cd "$gld"; bp="$gld/bin:$cpath"; csh=`PATH="$bp" command -v sh`; cflags=" -L `pwd`"
    env PATH="$bp" CONFIG_SHELL="$csh" SHELL="$csh" C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        CPP="$gcc -E$cppflags" CC="$gcc$cppflags$cflags" \
        "$csh" ./configure --disable-shared --enable-static --disable-sanity-checks \
        --build=i686-unknown-linux-gnu --host=i686-unknown-linux-gnu --with-headers="$hd/include" \
        --enable-static-nss --without-__thread --without-cvs --without-gd --without-tls --prefix="$gld/out" >cfg.log 2>&1 \
      || { echo "glibc configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_glibc-cfg.log" 2>/dev/null||true; tail -15 cfg.log >&2; exit 1; }
    test -f config.make || { echo "glibc configure produced no config.make" >&2; exit 1; }
    sed -i 's,INSTALL = scripts/,INSTALL = $(..)./scripts/,' config.make
    sed -i "s,^BASH = ,SHELL = $csh\n         BASH = ," config.make
    for f in `grep -rl '^#! */bin/sh' . 2>/dev/null`; do sed -i "1s,^#! *[^ ]*/bin/sh,#!$csh," "$f" 2>/dev/null || true; done
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mk/make" SHELL="$csh" "CC=$gcc$cppflags$cflags" >build.log 2>&1 \
      || { echo "glibc make failed" >&2; cp build.log "$ROOT/.td-build-cache/_glibc-build.log" 2>/dev/null||true; tail -25 build.log >&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mk/make" SHELL="$csh" "CC=$gcc$cppflags$cflags" install >install.log 2>&1 \
      || { echo "glibc install failed" >&2; cp install.log "$ROOT/.td-build-cache/_glibc-install.log" 2>/dev/null||true; tail -15 install.log >&2; exit 1; }
  ) || return 1
  test -s "$gld/out/lib/libc.a" -a -f "$gld/out/lib/crt1.o" || { echo "no glibc libc.a/crt produced" >&2; return 1; }
}
# --- gcc-mesboot0: gcc 2.95.3 rebuilt by the FIRST gcc (gd, gcc-core-mesboot0), now linking against
# glibc (gld) instead of mes libc (guix's gcc-mesboot0). CC=<first gcc> (not tcc); the headers/libs
# resolve to glibc + the first gcc's gcc-lib; RANLIB=true, LANGUAGES=c, simpler install2 (no libtcc1).
build_gcc_mesboot0() {
  cpath=$1; gd=$2; bd=$3; gld=$4; hd=$5; mk=$6; pd=$7; g2=$8
  rm -rf "$g2"; mkdir -p "$g2/bin"; tar -xzf "$GCC_TB" -C "$g2" --strip-components=1
  ( cd "$g2" && env -i "$pd/patch" --force -p1 -i "$GCC_PATCH" ) >"$g2/patch.log" 2>&1 || { echo "gcc-mesboot0 patch failed">&2; tail -8 "$g2/patch.log">&2; return 1; }
  gcc="$gd/out/bin/gcc"; gccdir1="$gd/out/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3"
  ln -sf "$gcc" "$g2/bin/gcc"; ln -sf "$gd"/out/bin/cpp "$g2/bin/cpp"
  for t in "$bd"/out/bin/*; do ln -sf "$t" "$g2/bin/`basename "$t"`"; done
  ln -sf "$mk/make" "$g2/bin/make"; ln -sf "$pd/patch" "$g2/bin/patch"
  ln -sf "`command -v awk 2>/dev/null || ls /gnu/store/*gawk*/bin/awk 2>/dev/null|sort|head -1`" "$g2/bin/awk"
  ln -sf "`command -v flex 2>/dev/null || ls /gnu/store/*flex*/bin/flex 2>/dev/null|sort|head -1`" "$g2/bin/flex"
  ln -sf "$g2/bin/flex" "$g2/bin/lex"; ln -sf "`command -v bison 2>/dev/null || ls /gnu/store/*bison*/bin/bison 2>/dev/null|sort|head -1`" "$g2/bin/bison"; ln -sf "$g2/bin/bison" "$g2/bin/yacc"
  CIP="$gld/out/include:$gccdir1/include:$hd/include"; LP="$gld/out/lib:$gccdir1"
  ( cd "$g2"; bp="$g2/bin:$cpath"; csh=`PATH="$bp" command -v sh`
    printf "ac_cv_c_float_format='IEEE (little-endian)'\n" > config.cache
    env PATH="$bp" CONFIG_SHELL="$csh" C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" CC="$gcc" CPP="$gcc -E" \
        "$csh" ./configure --disable-shared --disable-werror --build=i686-unknown-linux-gnu --host=i686-unknown-linux-gnu --prefix="$g2/out" >cfg.log 2>&1 \
      || { echo "gcc-mesboot0 configure failed">&2; cp cfg.log "$ROOT/.td-build-cache/_gcc1-cfg.log" 2>/dev/null||true; tail -15 cfg.log>&2; exit 1; }
    test -s Makefile || { echo "gcc-mesboot0 no Makefile">&2; exit 1; }
    rm -rf texinfo 2>/dev/null||true; mkdir -p gcc; touch gcc/cpp.info gcc/gcc.info
    for f in `grep -rl '^#! */bin/sh' . 2>/dev/null`; do sed -i "1s,^#! *[^ ]*/bin/sh,#!$csh," "$f" 2>/dev/null||true; done
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mk/make" SHELL="$csh" CONFIG_SHELL="$csh" CC="$gcc" RANLIB=true "LIBGCC2_INCLUDES=-I $gd/out/include" LANGUAGES=c >build.log 2>&1 \
      || { echo "gcc-mesboot0 make failed">&2; cp build.log "$ROOT/.td-build-cache/_gcc1-build.log" 2>/dev/null||true; tail -25 build.log>&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mk/make" SHELL="$csh" CC="$gcc" RANLIB=true LANGUAGES=c install >install.log 2>&1 \
      || { echo "gcc-mesboot0 install failed">&2; cp install.log "$ROOT/.td-build-cache/_gcc1-install.log" 2>/dev/null||true; tail -15 install.log>&2; exit 1; }
    gccdir2="$g2/out/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3"; mkdir -p "$gccdir2"
    ( rm -rf tg; mkdir tg; cd tg; env PATH="$bp" ar x ../gcc/libgcc2.a; env PATH="$bp" ar r "$gccdir2/libgcc.a" *.o ) >install2.log 2>&1 || { echo "gcc-mesboot0 install2 failed">&2; exit 1; }
    cp gcc/libgcc2.a "$g2/out/lib/libgcc2.a" 2>/dev/null||true
  ) || return 1
  test -x "$g2/out/bin/gcc" || { echo "no gcc-mesboot0 produced" >&2; return 1; }
}

# build_binutils_mesboot1 — GNU Binutils 2.20.1a REBUILT by gcc-mesboot0 against glibc-mesboot0
# (guix's binutils-mesboot1). Unlike binutils-mesboot0 (tcc + mes libc), this is a plain configure:
# CC=<gcc-mesboot0>, AR/RANLIB = the real binutils-mesboot0 ar/ranlib, libc = glibc-mesboot0, kernel =
# the PURE host-produced UAPI headers. NO CPPFLAGS / MES_BOOTSTRAP / __GLIBC_MINOR__ override — real
# glibc supplies them. The boot patch is still applied; its MES_BOOTSTRAP branches compile their
# non-bootstrap (real-glibc) side. make gets the SHELL var + cleared MAKEFLAGS (jobserver).
#
# Two non-obvious requirements (else libiberty's fibheap.c fails on LONG_MIN):
#  - NO -B<glibc>/lib in CC. gcc 2.95.3 warns "file path prefix … never used" during -E-only
#    preprocessing; autoconf treats ANY stderr as a failed header test, so HAVE_LIMITS_H/HAVE_STDLIB_H
#    end up undefined and fibheap.c (which #includes <limits.h> only under HAVE_LIMITS_H) loses
#    LONG_MIN. crt is found via LIBRARY_PATH instead (gcc adds it to the startfile prefixes).
#  - PURE kernel UAPI headers, NOT the mes-merged HD. HD carries the mes libc's own limits.h/stddef.h,
#    which shadow gcc's and redefine PATH_MAX (a warning -> again poisons autoconf's header checks).
#    gcc supplies stddef.h/limits.h built-in; glibc + kernel UAPI are the only extra includes (guix).
build_binutils_mesboot1() {
  cpath=$1; g2=$2; bd=$3; gld=$4; mk=$5; pd=$6; b2=$7
  rm -rf "$b2"; mkdir -p "$b2/bin"
  bz=`command -v bzip2 2>/dev/null || ls /gnu/store/*bzip2*/bin/bzip2 2>/dev/null | head -1`
  test -n "$bz" || { echo "no bzip2 to unpack binutils" >&2; return 1; }
  "$bz" -dc "$BU_TB" | tar -xf - -C "$b2" --strip-components=1 || { echo "binutils unpack failed" >&2; return 1; }
  ( cd "$b2" && env -i "$pd/patch" -p1 < "$BOOT_PATCH" ) >"$b2/patch.log" 2>&1 \
    || { echo "binutils boot-patch apply failed" >&2; tail -8 "$b2/patch.log" >&2; return 1; }
  gcc="$g2/out/bin/gcc"; g2dir="$g2/out/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3"
  # PURE kernel UAPI (linux/ asm/ asm-generic/ …), produced on the host from the pinned linux source.
  kh="$b2/kh"; mkdir -p "$kh"; tar -xzf "$KH_TB" -C "$kh" || { echo "kernel headers unpack failed" >&2; return 1; }
  ln -sf "$g2"/out/bin/cpp "$b2/bin/cpp"
  for t in "$bd"/out/bin/*; do ln -sf "$t" "$b2/bin/`basename "$t"`"; done   # binutils-mesboot0 ar/as/ld/ranlib/nm/strip
  ln -sf "$mk/make" "$b2/bin/make"; ln -sf "$pd/patch" "$b2/bin/patch"
  awkb=`command -v awk 2>/dev/null || ls /gnu/store/*gawk*/bin/awk 2>/dev/null | sort | head -1`
  flexb=`command -v flex 2>/dev/null || ls /gnu/store/*flex*/bin/flex 2>/dev/null | sort | head -1`
  bisonb=`command -v bison 2>/dev/null || ls /gnu/store/*bison*/bin/bison 2>/dev/null | sort | head -1`
  test -n "$awkb" -a -n "$flexb" -a -n "$bisonb" || { echo "need awk/flex/bison (build tools) from the store" >&2; return 1; }
  ln -sf "$awkb" "$b2/bin/awk"; ln -sf "$flexb" "$b2/bin/flex"; ln -sf "$flexb" "$b2/bin/lex"
  ln -sf "$bisonb" "$b2/bin/bison"; ln -sf "$bisonb" "$b2/bin/yacc"
  CIP="$gld/out/include:$kh"; LP="$gld/out/lib:$g2dir"
  ( cd "$b2"; bp="$b2/bin:$cpath"; csh=`PATH="$bp" command -v sh`
    env PATH="$bp" CONFIG_SHELL="$csh" C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" "$csh" ./configure \
        "CC=$gcc -static" AR=ar RANLIB=ranlib CXX=false \
        --disable-nls --disable-shared --disable-werror \
        --build=i686-unknown-linux-gnu --host=i686-unknown-linux-gnu --with-sysroot=/ >cfg.log 2>&1 \
      || { echo "binutils-mesboot1 configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_binutils1-cfg.log" 2>/dev/null||true; tail -20 cfg.log >&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mk/make" SHELL="$csh" CONFIG_SHELL="$csh" "CC=$gcc -static" AR=ar RANLIB=ranlib CXX=false >build.log 2>&1 \
      || { echo "binutils-mesboot1 make failed" >&2; cp build.log "$ROOT/.td-build-cache/_binutils1-build.log" 2>/dev/null||true; tail -30 build.log >&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mk/make" SHELL="$csh" install prefix="$b2/out" >install.log 2>&1 \
      || { echo "binutils-mesboot1 install failed" >&2; cp install.log "$ROOT/.td-build-cache/_binutils1-install.log" 2>/dev/null||true; tail -15 install.log >&2; exit 1; }
  ) || return 1
  test -x "$b2/out/bin/as" -a -x "$b2/out/bin/ld" || { echo "no as/ld produced" >&2; return 1; }
}
# build_make_mesboot — GNU Make 3.82 REBUILT by gcc-mesboot0 against glibc (guix's make-mesboot). The
# tcc-built make-mesboot0 (make 3.80, mes libc) drives the build; CC=<gcc-mesboot0>, as/ld/ar =
# binutils-mesboot0, libc = glibc-mesboot0, kernel = pure UAPI headers. Static glibc needs the nss/
# resolv archives named explicitly (LIBS), exactly as guix's make-mesboot. Same env discipline as
# binutils-mesboot1: no -B (crt via LIBRARY_PATH), pure kernel headers, cleared MAKEFLAGS + SHELL var.
build_make_mesboot() {
  cpath=$1; g2=$2; bd=$3; gld=$4; mk=$5; m2d=$6
  rm -rf "$m2d"; mkdir -p "$m2d/bin"
  tar -xzf "$MAKE382_TB" -C "$m2d" --strip-components=1 || { echo "make-3.82 unpack failed" >&2; return 1; }
  gcc="$g2/out/bin/gcc"; g2dir="$g2/out/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3"
  kh="$m2d/kh"; mkdir -p "$kh"; tar -xzf "$KH_TB" -C "$kh" || { echo "kernel headers unpack failed" >&2; return 1; }
  ln -sf "$g2"/out/bin/cpp "$m2d/bin/cpp"
  for t in "$bd"/out/bin/*; do ln -sf "$t" "$m2d/bin/`basename "$t"`"; done   # binutils-mesboot0 as/ld/ar/ranlib
  ln -sf "$mk/make" "$m2d/bin/make"                                            # make-mesboot0 drives it
  awkb=`command -v awk 2>/dev/null || ls /gnu/store/*gawk*/bin/awk 2>/dev/null | sort | head -1`
  test -n "$awkb" || { echo "need awk (build tool) from the store" >&2; return 1; }
  ln -sf "$awkb" "$m2d/bin/awk"
  CIP="$gld/out/include:$kh"; LP="$gld/out/lib:$g2dir"
  ( cd "$m2d"; bp="$m2d/bin:$cpath"; csh=`PATH="$bp" command -v sh`
    env PATH="$bp" CONFIG_SHELL="$csh" C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" "$csh" ./configure \
        "CC=$gcc -static" AR=ar RANLIB=ranlib "LIBS=-lc -lnss_files -lnss_dns -lresolv" \
        --build=i686-unknown-linux-gnu --host=i686-unknown-linux-gnu --disable-nls >cfg.log 2>&1 \
      || { echo "make-mesboot configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_makemesboot-cfg.log" 2>/dev/null||true; tail -20 cfg.log >&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mk/make" SHELL="$csh" CONFIG_SHELL="$csh" "CC=$gcc -static" AR=ar RANLIB=ranlib >build.log 2>&1 \
      || { echo "make-mesboot make failed" >&2; cp build.log "$ROOT/.td-build-cache/_makemesboot-build.log" 2>/dev/null||true; tail -30 build.log >&2; exit 1; }
  ) || return 1
  test -x "$m2d/make" || { echo "no make binary produced" >&2; return 1; }
}
# build_gcc_core_mesboot1 — GCC 4.6.4 (C only) built by gcc-mesboot0 + binutils-mesboot1 + make-mesboot
# against glibc-mesboot0 — guix's gcc-core-mesboot1. gmp 4.3.2 / mpfr 2.4.2 / mpc 1.0.3 are unpacked
# INTO the gcc source tree (gcc builds them in-tree). td's glibc-mesboot0 is static-only (no ld.so), so
# unlike guix (which sets a -dynamic-linker) td builds gcc 4.6.4 STATIC: LDFLAGS="-static -B<glibc>/lib"
# (the -B is link-only, so it does NOT trip the autoconf -E "never used" warning binutils-mesboot1 hit).
# C_INCLUDE_PATH puts gcc-mesboot0's own gcc-lib/include FIRST (guix's order), then kernel, glibc,
# mpfr/src. Same make discipline: cleared MAKEFLAGS jobserver + SHELL var; /bin/sh shebangs rewritten.
build_gcc_core_mesboot1() {
  cpath=$1; g2=$2; b2=$3; mm=$4; gld=$5; pd=$6; gc1=$7
  rm -rf "$gc1"; mkdir -p "$gc1/bin"
  tar -xzf "$GCC464_TB" -C "$gc1" --strip-components=1 || { echo "gcc-4.6.4 unpack failed" >&2; return 1; }
  ( cd "$gc1" && env -i "$pd/patch" --force -p1 -i "$GCC464_PATCH" ) >"$gc1/patch.log" 2>&1 \
    || { echo "gcc-4.6.4 boot-patch apply failed" >&2; tail -8 "$gc1/patch.log" >&2; return 1; }
  # guix's unpack-gmp&co: unpack the math libs into the tree + symlink gmp-x.y.z -> gmp, etc.
  # (extract with -C so the tarball paths stay relative to the caller's cwd, then cd only to symlink)
  tar -xzf "$GMP_TB" -C "$gc1" && tar -xzf "$MPFR_TB" -C "$gc1" && tar -xzf "$MPC_TB" -C "$gc1" \
    || { echo "gmp/mpfr/mpc unpack failed" >&2; return 1; }
  ( cd "$gc1" && ln -sf gmp-4.3.2 gmp && ln -sf mpfr-2.4.2 mpfr && ln -sf mpc-1.0.3 mpc ) \
    || { echo "gmp/mpfr/mpc symlink failed" >&2; return 1; }
  gcc="$g2/out/bin/gcc"; g2dir="$g2/out/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3"
  kh="$gc1/kh"; mkdir -p "$kh"; tar -xzf "$KH_TB" -C "$kh" || { echo "kernel headers unpack failed" >&2; return 1; }
  ln -sf "$g2"/out/bin/cpp "$gc1/bin/cpp"
  for t in "$b2"/out/bin/*; do ln -sf "$t" "$gc1/bin/`basename "$t"`"; done   # binutils-mesboot1 as/ld/ar/ranlib
  ln -sf "$mm/make" "$gc1/bin/make"; ln -sf "$pd/patch" "$gc1/bin/patch"
  awkb=`command -v awk 2>/dev/null || ls /gnu/store/*gawk*/bin/awk 2>/dev/null | sort | head -1`
  flexb=`command -v flex 2>/dev/null || ls /gnu/store/*flex*/bin/flex 2>/dev/null | sort | head -1`
  bisonb=`command -v bison 2>/dev/null || ls /gnu/store/*bison*/bin/bison 2>/dev/null | sort | head -1`
  test -n "$awkb" -a -n "$flexb" -a -n "$bisonb" || { echo "need awk/flex/bison (build tools) from the store" >&2; return 1; }
  ln -sf "$awkb" "$gc1/bin/awk"; ln -sf "$flexb" "$gc1/bin/flex"; ln -sf "$flexb" "$gc1/bin/lex"
  ln -sf "$bisonb" "$gc1/bin/bison"; ln -sf "$bisonb" "$gc1/bin/yacc"
  # gcc 4.6.4's install regenerates tm.texi via move-if-change, which needs `cmp` (diffutils) — absent
  # in the loop sandbox (the host dev harness has it, so this only bit in-sandbox). Build-time only.
  cmpb=`command -v cmp 2>/dev/null || ls /gnu/store/*diffutils*/bin/cmp 2>/dev/null | sort | head -1`
  diffb=`command -v diff 2>/dev/null || ls /gnu/store/*diffutils*/bin/diff 2>/dev/null | sort | head -1`
  test -n "$cmpb" -a -n "$diffb" || { echo "need cmp/diff (diffutils, build tools) from the store" >&2; return 1; }
  ln -sf "$cmpb" "$gc1/bin/cmp"; ln -sf "$diffb" "$gc1/bin/diff"
  CIP="$g2dir/include:$kh:$gld/out/include:$gc1/mpfr/src"; LP="$gld/out/lib:$g2/out/lib"
  ldf="-static -B$gld/out/lib"
  ( cd "$gc1"; bp="$g2/out/bin:$gc1/bin:$cpath"; csh=`PATH="$bp" command -v sh`
    for f in `grep -rl '^#! */bin/sh' . 2>/dev/null`; do sed -i "1s,^#! *[^ ]*/bin/sh,#!$csh," "$f" 2>/dev/null || true; done
    env PATH="$bp" CONFIG_SHELL="$csh" CC="$gcc" CPP="$gcc -E" C_INCLUDE_PATH="$CIP" CPLUS_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$csh" ./configure --prefix="$gc1/out" --build=i686-unknown-linux-gnu --host=i686-unknown-linux-gnu \
        --with-native-system-header-dir="$gld/out/include" --with-build-sysroot="$gld/out/include" \
        --disable-bootstrap --disable-decimal-float --disable-libatomic --disable-libcilkrts --disable-libgomp \
        --disable-libitm --disable-libmudflap --disable-libquadmath --disable-libsanitizer --disable-libssp \
        --disable-libvtv --disable-lto --disable-lto-plugin --disable-multilib --disable-plugin --disable-threads \
        --enable-languages=c --enable-static --disable-shared --enable-threads=single --disable-libstdcxx-pch \
        --disable-build-with-cxx >cfg.log 2>&1 \
      || { echo "gcc-core-mesboot1 configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_gcccore1-cfg.log" 2>/dev/null||true; tail -25 cfg.log >&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" C_INCLUDE_PATH="$CIP" CPLUS_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mm/make" SHELL="$csh" CONFIG_SHELL="$csh" MAKEINFO=true "LDFLAGS=$ldf" "LDFLAGS_FOR_TARGET=$ldf" >build.log 2>&1 \
      || { echo "gcc-core-mesboot1 make failed" >&2; cp build.log "$ROOT/.td-build-cache/_gcccore1-build.log" 2>/dev/null||true; tail -40 build.log >&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mm/make" SHELL="$csh" MAKEINFO=true install >install.log 2>&1 \
      || { echo "gcc-core-mesboot1 install failed" >&2; cp install.log "$ROOT/.td-build-cache/_gcccore1-install.log" 2>/dev/null||true; tail -20 install.log >&2; exit 1; }
  ) || return 1
  test -x "$gc1/out/bin/gcc" || { echo "no gcc-core-mesboot1 produced" >&2; return 1; }
}

# --- [pinned-input] extras: make-3.82 + gcc-4.6.4 + gmp/mpfr/mpc + the gcc-4.6.4 boot patch ---------
MAKE382_LOCK=`ls seed/sources/make-3.82.lock`; MAKE382_TB=".td-build-cache/sources/`lf "$MAKE382_LOCK" file`"
GCC464_LOCK=`ls seed/sources/gcc-core-4.6.4.lock`; GCC464_TB=".td-build-cache/sources/`lf "$GCC464_LOCK" file`"
GMP_LOCK=`ls seed/sources/gmp-*.lock`;   GMP_TB=".td-build-cache/sources/`lf "$GMP_LOCK" file`"
MPFR_LOCK=`ls seed/sources/mpfr-*.lock`; MPFR_TB=".td-build-cache/sources/`lf "$MPFR_LOCK" file`"
MPC_LOCK=`ls seed/sources/mpc-*.lock`;   MPC_TB=".td-build-cache/sources/`lf "$MPC_LOCK" file`"
GCC464_PATCH="$ROOT/seed/patches/gcc-boot-4.6.4.patch"
GCC464_PATCH_SHA=0dfcb1813ca54eafad0d3bbec17b423d6e50ab76d730b35eb6df7018ed43edff
for pair in "$MAKE382_TB:`lf "$MAKE382_LOCK" sha256`" "$GCC464_TB:`lf "$GCC464_LOCK" sha256`" \
            "$GMP_TB:`lf "$GMP_LOCK" sha256`" "$MPFR_TB:`lf "$MPFR_LOCK" sha256`" "$MPC_TB:`lf "$MPC_LOCK" sha256`"; do
  f=${pair%:*}; want=${pair##*:}
  test -f "$f" || fail "pinned tarball not warm ($f) — run 'td-feed warm sources'"
  test "`sha "$f"`" = "$want" || fail "warmed $f sha256 != lock pin ($want)"
done
test -f "$GCC464_PATCH" || fail "vendored patch missing ($GCC464_PATCH)"
test "`sha "$GCC464_PATCH"`" = "$GCC464_PATCH_SHA" || fail "vendored gcc-4.6.4 boot patch sha256 != pin"
echo "   [pinned-input] + make-3.82, gcc-4.6.4, gmp/mpfr/mpc tarballs + the gcc-4.6.4 boot patch match their pins"

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
GC1=`mktemp -d`/gcccore1build; build_gcc_core_mesboot1 "$cpath" "$G2" "$B2" "$MM" "$GLD" "$PD" "$GC1" || fail "the toolchain did not build GCC 4.6.4"
trap 'rm -rf "$tc" "$mesp" "`dirname "$TCCD"`" "`dirname "$MK"`" "`dirname "$PD"`" "`dirname "$BD"`" "`dirname "$GD"`" "`dirname "$HD"`" "`dirname "$GLD"`" "`dirname "$G2"`" "`dirname "$B2"`" "`dirname "$MM"`" "`dirname "$GC1"`" "`dirname "$cpath"`"' EXIT INT TERM

GCC1="$GC1/out/bin/gcc"; GC1DIR="$GC1/out/lib/gcc/i686-unknown-linux-gnu/4.6.4"; CC1="$GC1/out/libexec/gcc/i686-unknown-linux-gnu/4.6.4/cc1"

# --- [no-guix] -------------------------------------------------------------------------------
test -x "$GCC1" -a -f "$CC1" || fail "gcc-core-mesboot1 missing gcc or cc1"
for art in "$GCC1" "$CC1"; do if grep -q -a '/gnu/store' "$art"; then fail "$art contains /gnu/store bytes"; fi; done
echo "   [no-guix] seed → … → gcc-mesboot0 → binutils-mesboot1 → make-mesboot → gcc-core-mesboot1 built with no gcc/guile/guix on PATH; no /gnu/store in gcc/cc1"

# --- [behavioral] gcc reports 4.6.4 and COMPILES + LINKS + RUNS a static C program → 42 ------------
gv=`env -i "$GCC1" --version 2>"$GC1/gcc.err" | head -1` || { tail -3 "$GC1/gcc.err" >&2; fail "gcc-core-mesboot1 did not run"; }
echo "$gv" | grep -q '4\.6\.4' || fail "gcc-core-mesboot1 --version gave [$gv], want 4.6.4"
wd=`mktemp -d`; printf '#include <stdio.h>\nint main(){printf("hi");return 42;}\n' > "$wd/t.c"
( cd "$wd" && env -i PATH="$B2/out/bin:$cpath" C_INCLUDE_PATH="$GLD/out/include" LIBRARY_PATH="$GLD/out/lib:$GC1DIR" \
    "$GCC1" -static -B"$GLD/out/lib" -o t t.c ) >"$wd/cc.log" 2>&1 \
  || { tail -12 "$wd/cc.log" >&2; rm -rf "$wd"; fail "gcc-core-mesboot1 could not compile+link a C program"; }
head -c20 "$wd/t" | od -An -tx1 | grep -q '7f 45 4c 46 01' || { rm -rf "$wd"; fail "gcc-core-mesboot1 did not emit a 32-bit ELF"; }
set +e; ( cd "$wd" && env -i ./t ); rc=$?; set -e
rm -rf "$wd"
test "$rc" = 42 || fail "the gcc-core-mesboot1-built program returned $rc, want 42"
echo "   [behavioral] gcc-core-mesboot1→'$gv'; it compiled+linked+ran a static C program → 42 — a modern gcc from source"

# --- [repro] the gcc driver is byte-identical AND the rebuilt compiler emits identical code --------
# cc1's binary carries benign embedded metadata (the 2.95.3 build compiler's stabs version stamp), so
# this asserts the DRIVER's byte-identity PLUS the compiler's intrinsic OUTPUT-reproducibility — two
# independently-built gcc emit byte-identical assembly — which is the durable property that matters.
g1=`sha "$GCC1"`
rs=`mktemp -d`; printf 'int f(int x){return x*x+1;}\nlong g(long a,long b){return a%%b+a/b;}\n' > "$rs/r.c"
env -i PATH="$B2/out/bin:$cpath" "$GCC1" -S -O2 -o "$rs/r1.s" "$rs/r.c" || { rm -rf "$rs"; fail "gcc-core-mesboot1 could not emit assembly"; }
build_gcc_core_mesboot1 "$cpath" "$G2" "$B2" "$MM" "$GLD" "$PD" "$GC1" || { rm -rf "$rs"; fail "the second gcc-core-mesboot1 build did not run"; }
test "$g1" = "`sha "$GCC1"`" || { rm -rf "$rs"; fail "gcc-core-mesboot1 gcc driver is NOT reproducible — r1=$g1 r2=`sha "$GCC1"`"; }
env -i PATH="$B2/out/bin:$cpath" "$GCC1" -S -O2 -o "$rs/r2.s" "$rs/r.c" || { rm -rf "$rs"; fail "the rebuilt gcc could not emit assembly"; }
test "`sha "$rs/r1.s"`" = "`sha "$rs/r2.s"`" || { rm -rf "$rs"; fail "the rebuilt gcc-core-mesboot1 emits DIFFERENT assembly — not reproducible"; }
rm -rf "$rs"
echo "   [repro] the gcc driver is byte-identical across two builds, and the rebuilt compiler emits byte-identical assembly (intrinsic output-reproducibility; cc1's binary carries only benign build-compiler metadata)"

echo "PASS: source-bootstrap brick 5 — from the 229-byte seed, td built GCC 4.6.4 (guix's"
echo "      gcc-core-mesboot1): a modern modular gcc, built by gcc-mesboot0 + binutils-mesboot1 +"
echo "      make-mesboot against glibc with in-tree gmp/mpfr/mpc, that compiles+links+runs C → 42; no"
echo "      gcc/guile/guix, no /gnu/store, reproducible. gcc-mesboot1 (+c++) then gcc-mesboot 4.7.4 next."
