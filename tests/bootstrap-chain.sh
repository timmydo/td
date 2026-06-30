#!/bin/sh
# tests/bootstrap-chain.sh — the SHARED from-seed modern-toolchain chain, sourced by the
# bootstrap-*-store-native gates (extracted verbatim from the per-gate inline chain so the ~850-line
# seed→…→gcc-14.3.0+binutils-2.44+glibc-2.41 build lives in ONE place). The caller sets `set -eu` +
# ROOT=$(pwd), sources this, then calls `bootstrap_modern_toolchain`, which builds the whole chain
# from the 229-byte seed, verifies it carries no /gnu/store, and leaves these GLOBALS set for the gate:
#   GCC14        = the gcc 14.3.0 prefix (…/stage/td/store/gcc-14.3.0)
#   GLIBC241     = the modern glibc 2.41 prefix (ld-scripts relocated + kernel headers added)
#   BMB244SB     = the sandbox-runnable binutils 2.44 build dir (…/out/bin has as/ld/readelf)
#   CC1, cpath, KH_TB + the intermediate build dirs (the EXIT trap cleans them).
# This is a pure code-move: the gates that source it build the IDENTICAL toolchain they did inline.

fail() { echo "FAIL: $*" >&2; exit 1; }
# Parallelism for the MODERN rungs (gcc 14 / glibc 2.41 / binutils 2.44 — robust guix make with MAKEFLAGS
# cleared, so -j gets a FRESH jobserver). The early mesboot rungs (tcc-built mes-libc make) stay serial.
BJOBS=${TD_BUILD_CORES:-$(nproc 2>/dev/null || echo 4)}
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
lf() { sed -n "s/^$2 //p" "$1" | head -1; }

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
# --- mesboot-headers: install the host-produced Linux UAPI headers (td-feed warm sources) + the mes
# includes (guix's mesboot-headers merges both). The sandbox can't run the kernel build; the headers
# tarball is produced on the host from the pinned linux source. Returns the headers dir.
build_headers() {
  mesp=$1; hd=$2
  rm -rf "$hd"; mkdir -p "$hd/include"
  test -f "$KH_TB" || { echo "kernel headers tarball not produced ($KH_TB) — run 'td-feed warm sources'" >&2; return 1; }
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
# build_gcc_mesboot1 — GCC 4.6.4 with C AND C++ (guix's gcc-mesboot1) — gcc-core-mesboot1 plus the
# gcc-g++-4.6.4 front-end overlaid into the tree + --enable-languages=c,c++ (builds cc1plus + a static
# libstdc++). The c++ compiler the next gcc (gcc-mesboot 4.7.4, itself C++) needs. Same toolchain +
# static + MAKEINFO=true + cmp/diff + env discipline as gcc-core-mesboot1; CPLUS_INCLUDE_PATH mirrors
# C_INCLUDE_PATH so the C headers are found while libstdc++ is compiled (guix's setenv).
build_gcc_mesboot1() {
  cpath=$1; g2=$2; b2=$3; mm=$4; gld=$5; pd=$6; gc1=$7
  rm -rf "$gc1"; mkdir -p "$gc1/bin"
  tar -xzf "$GCC464_TB" -C "$gc1" --strip-components=1 || { echo "gcc-4.6.4 unpack failed" >&2; return 1; }
  tar -xzf "$GPP464_TB" -C "$gc1" --strip-components=1 || { echo "gcc-g++-4.6.4 overlay unpack failed" >&2; return 1; }
  ( cd "$gc1" && env -i "$pd/patch" --force -p1 -i "$GCC464_PATCH" ) >"$gc1/patch.log" 2>&1 \
    || { echo "gcc-4.6.4 boot-patch apply failed" >&2; tail -8 "$gc1/patch.log" >&2; return 1; }
  tar -xzf "$GMP_TB" -C "$gc1" && tar -xzf "$MPFR_TB" -C "$gc1" && tar -xzf "$MPC_TB" -C "$gc1" \
    || { echo "gmp/mpfr/mpc unpack failed" >&2; return 1; }
  ( cd "$gc1" && ln -sf gmp-4.3.2 gmp && ln -sf mpfr-2.4.2 mpfr && ln -sf mpc-1.0.3 mpc ) \
    || { echo "gmp/mpfr/mpc symlink failed" >&2; return 1; }
  gcc="$g2/out/bin/gcc"; g2dir="$g2/out/lib/gcc-lib/i686-unknown-linux-gnu/2.95.3"
  kh="$gc1/kh"; mkdir -p "$kh"; tar -xzf "$KH_TB" -C "$kh" || { echo "kernel headers unpack failed" >&2; return 1; }
  ln -sf "$g2"/out/bin/cpp "$gc1/bin/cpp"
  for t in "$b2"/out/bin/*; do ln -sf "$t" "$gc1/bin/`basename "$t"`"; done
  ln -sf "$mm/make" "$gc1/bin/make"; ln -sf "$pd/patch" "$gc1/bin/patch"
  awkb=`command -v awk 2>/dev/null || ls /gnu/store/*gawk*/bin/awk 2>/dev/null | sort | head -1`
  flexb=`command -v flex 2>/dev/null || ls /gnu/store/*flex*/bin/flex 2>/dev/null | sort | head -1`
  bisonb=`command -v bison 2>/dev/null || ls /gnu/store/*bison*/bin/bison 2>/dev/null | sort | head -1`
  cmpb=`command -v cmp 2>/dev/null || ls /gnu/store/*diffutils*/bin/cmp 2>/dev/null | sort | head -1`
  diffb=`command -v diff 2>/dev/null || ls /gnu/store/*diffutils*/bin/diff 2>/dev/null | sort | head -1`
  test -n "$awkb" -a -n "$flexb" -a -n "$bisonb" -a -n "$cmpb" -a -n "$diffb" || { echo "need awk/flex/bison/cmp/diff (build tools) from the store" >&2; return 1; }
  ln -sf "$awkb" "$gc1/bin/awk"; ln -sf "$flexb" "$gc1/bin/flex"; ln -sf "$flexb" "$gc1/bin/lex"
  ln -sf "$bisonb" "$gc1/bin/bison"; ln -sf "$bisonb" "$gc1/bin/yacc"
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
        --enable-languages=c,c++ --enable-static --disable-shared --enable-threads=single --disable-libstdcxx-pch \
        --disable-build-with-cxx >cfg.log 2>&1 \
      || { echo "gcc-mesboot1 configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_gccmesboot1-cfg.log" 2>/dev/null||true; tail -25 cfg.log >&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" C_INCLUDE_PATH="$CIP" CPLUS_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mm/make" SHELL="$csh" CONFIG_SHELL="$csh" MAKEINFO=true "LDFLAGS=$ldf" "LDFLAGS_FOR_TARGET=$ldf" >build.log 2>&1 \
      || { echo "gcc-mesboot1 make failed" >&2; cp build.log "$ROOT/.td-build-cache/_gccmesboot1-build.log" 2>/dev/null||true; tail -40 build.log >&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" C_INCLUDE_PATH="$CIP" CPLUS_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mm/make" SHELL="$csh" MAKEINFO=true install >install.log 2>&1 \
      || { echo "gcc-mesboot1 install failed" >&2; cp install.log "$ROOT/.td-build-cache/_gccmesboot1-install.log" 2>/dev/null||true; tail -20 install.log >&2; exit 1; }
  ) || return 1
  test -x "$gc1/out/bin/gcc" -a -x "$gc1/out/bin/g++" || { echo "no gcc/g++ produced" >&2; return 1; }
}
# build_binutils_mesboot — GNU Binutils 2.20.1a rebuilt by gcc-mesboot1 (GCC 4.6.4) against glibc —
# guix's binutils-mesboot (= binutils-mesboot1 with gcc-mesboot1 as CC). Same plain configure as
# binutils-mesboot1; the difference is CC = the c++-capable gcc 4.6.4. gcc 4.6.4's gcc-lib lives at
# out/lib/gcc/<triplet>/4.6.4 (not lib/gcc-lib/.../2.95.3).
build_binutils_mesboot() {
  cpath=$1; gm1=$2; b2=$3; gld=$4; mk=$5; pd=$6; bo=$7
  rm -rf "$bo"; mkdir -p "$bo/bin"
  bz=`command -v bzip2 2>/dev/null || ls /gnu/store/*bzip2*/bin/bzip2 2>/dev/null | head -1`
  test -n "$bz" || { echo "no bzip2" >&2; return 1; }
  "$bz" -dc "$BU_TB" | tar -xf - -C "$bo" --strip-components=1 || { echo "binutils unpack failed" >&2; return 1; }
  ( cd "$bo" && env -i "$pd/patch" -p1 < "$BOOT_PATCH" ) >"$bo/patch.log" 2>&1 \
    || { echo "binutils boot-patch apply failed" >&2; tail -8 "$bo/patch.log" >&2; return 1; }
  gcc="$gm1/out/bin/gcc"; gccdir="$gm1/out/lib/gcc/i686-unknown-linux-gnu/4.6.4"
  kh="$bo/kh"; mkdir -p "$kh"; tar -xzf "$KH_TB" -C "$kh" || { echo "kernel headers unpack failed" >&2; return 1; }
  ln -sf "$gm1"/out/bin/cpp "$bo/bin/cpp"
  for t in "$b2"/out/bin/*; do ln -sf "$t" "$bo/bin/`basename "$t"`"; done
  ln -sf "$mk/make" "$bo/bin/make"; ln -sf "$pd/patch" "$bo/bin/patch"
  for tool in awk:gawk flex:flex bison:bison cmp:diffutils diff:diffutils; do
    n=${tool%%:*}; pk=${tool##*:}; b=`command -v "$n" 2>/dev/null || ls /gnu/store/*$pk*/bin/$n 2>/dev/null | sort | head -1`
    test -n "$b" || { echo "need $n from the store" >&2; return 1; }; ln -sf "$b" "$bo/bin/$n"
  done
  ln -sf "$bo/bin/flex" "$bo/bin/lex"; ln -sf "$bo/bin/bison" "$bo/bin/yacc"
  CIP="$gld/out/include:$kh"; LP="$gld/out/lib:$gccdir"
  ( cd "$bo"; bp="$gm1/out/bin:$bo/bin:$cpath"; csh=`PATH="$bp" command -v sh`
    env PATH="$bp" CONFIG_SHELL="$csh" C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" "$csh" ./configure \
        "CC=$gcc -static" AR=ar RANLIB=ranlib CXX=false --disable-nls --disable-shared --disable-werror \
        --build=i686-unknown-linux-gnu --host=i686-unknown-linux-gnu --with-sysroot=/ >cfg.log 2>&1 \
      || { echo "binutils-mesboot configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_binutilsmesboot-cfg.log" 2>/dev/null||true; tail -20 cfg.log >&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mk/make" SHELL="$csh" CONFIG_SHELL="$csh" "CC=$gcc -static" AR=ar RANLIB=ranlib CXX=false >build.log 2>&1 \
      || { echo "binutils-mesboot make failed" >&2; cp build.log "$ROOT/.td-build-cache/_binutilsmesboot-build.log" 2>/dev/null||true; tail -30 build.log >&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mk/make" SHELL="$csh" install prefix="$bo/out" >install.log 2>&1 \
      || { echo "binutils-mesboot install failed" >&2; cp install.log "$ROOT/.td-build-cache/_binutilsmesboot-install.log" 2>/dev/null||true; tail -15 install.log >&2; exit 1; }
  ) || return 1
  test -x "$bo/out/bin/as" -a -x "$bo/out/bin/ld" || { echo "no as/ld produced" >&2; return 1; }
}

# build_gawk_mesboot — GNU awk 3.1.8 built by gcc-mesboot1 against glibc — guix's gawk-mesboot. Needed
# because glibc-mesboot 2.16.0's versions.awk is too complex for the seed's gash-utils awk. Plain
# configure (ac_cv_func_connect=no — no sockets), make just the `gawk` binary; static glibc names its
# nss/resolv archives explicitly (LIBS), as for make-mesboot. install gawk + an awk symlink.
build_gawk_mesboot() {
  cpath=$1; gm1=$2; b2=$3; gld=$4; mk=$5; go=$6
  rm -rf "$go"; mkdir -p "$go/bin"
  tar -xzf "$GAWK_TB" -C "$go" --strip-components=1 || { echo "gawk unpack failed" >&2; return 1; }
  gcc="$gm1/out/bin/gcc"; gccdir="$gm1/out/lib/gcc/i686-unknown-linux-gnu/4.6.4"
  kh="$go/kh"; mkdir -p "$kh"; tar -xzf "$KH_TB" -C "$kh" || { echo "kernel headers unpack failed" >&2; return 1; }
  ln -sf "$gm1"/out/bin/cpp "$go/bin/cpp"
  for t in "$b2"/out/bin/*; do ln -sf "$t" "$go/bin/`basename "$t"`"; done
  ln -sf "$mk/make" "$go/bin/make"
  for tool in awk:gawk flex:flex bison:bison cmp:diffutils diff:diffutils; do
    n=${tool%%:*}; pk=${tool##*:}; b=`command -v "$n" 2>/dev/null || ls /gnu/store/*$pk*/bin/$n 2>/dev/null | sort | head -1`
    test -n "$b" || { echo "need $n from the store" >&2; return 1; }; ln -sf "$b" "$go/bin/$n"
  done
  ln -sf "$go/bin/flex" "$go/bin/lex"; ln -sf "$go/bin/bison" "$go/bin/yacc"
  CIP="$gld/out/include:$kh"; LP="$gld/out/lib:$gccdir"
  ( cd "$go"; bp="$gm1/out/bin:$go/bin:$cpath"; csh=`PATH="$bp" command -v sh`
    env PATH="$bp" CONFIG_SHELL="$csh" C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" "$csh" ./configure \
        "CC=$gcc -static" AR=ar RANLIB=ranlib ac_cv_func_connect=no "LIBS=-lc -lnss_files -lnss_dns -lresolv" \
        --build=i686-unknown-linux-gnu --host=i686-unknown-linux-gnu --disable-nls >cfg.log 2>&1 \
      || { echo "gawk-mesboot configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_gawkmesboot-cfg.log" 2>/dev/null||true; tail -20 cfg.log >&2; exit 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mk/make" SHELL="$csh" CONFIG_SHELL="$csh" "CC=$gcc -static" AR=ar RANLIB=ranlib gawk >build.log 2>&1 \
      || { echo "gawk-mesboot make failed" >&2; cp build.log "$ROOT/.td-build-cache/_gawkmesboot-build.log" 2>/dev/null||true; tail -30 build.log >&2; exit 1; }
  ) || return 1
  test -x "$go/gawk" || { echo "no gawk produced" >&2; return 1; }
  mkdir -p "$go/out/bin"; cp "$go/gawk" "$go/out/bin/gawk"; ln -sf gawk "$go/out/bin/awk"
}
# build_glibc_mesboot — GNU C Library 2.16.0 (guix's glibc-headers-mesboot + glibc-mesboot) built by
# gcc-mesboot1 + binutils-mesboot + gawk-mesboot. Two stages in one source tree: (A) install the
# bootstrap glibc headers (--with-headers=<kernel UAPI>, make install-bootstrap-headers), then (B) the
# full nptl library (--with-headers=<the stage-A glibc headers>, make + make install). Static; the exact
# guix env: CC=<gcc-mesboot1> with -I <src>/nptl/sysdeps/pthread/bits -D BOOTSTRAP_GLIBC=1 -L <src>
# -L <glibc-mesboot0>/lib; libc_cv_friendly_stddef=yes; libc_cv_ssp=false; the remove-bashism +
# remove-sunrpc fixups; configure in a build/ subdir; /bin/pwd -> pwd.
build_glibc_mesboot() {
  cpath=$1; gm1=$2; bmb=$3; gawk=$4; gld=$5; mk=$6; pd=$7; out=$8
  rm -rf "$out"; mkdir -p "$out/bin"
  src="$out/src"; mkdir -p "$src"; tar -xzf "$GLIBC216_TB" -C "$src" --strip-components=1 || { echo "glibc-2.16.0 unpack failed" >&2; return 1; }
  ( cd "$src" && env -i "$pd/patch" --force -p1 -i "$GLIBC216_P1" && env -i "$pd/patch" --force -p1 -i "$GLIBC216_P2" ) >"$out/patch.log" 2>&1 \
    || { echo "glibc-2.16.0 boot-patch apply failed" >&2; tail -8 "$out/patch.log" >&2; return 1; }
  gcc="$gm1/out/bin/gcc"
  kh="$out/kh"; mkdir -p "$kh"; tar -xzf "$KH_TB" -C "$kh" || { echo "kernel headers unpack failed" >&2; return 1; }
  ln -sf "$gm1"/out/bin/cpp "$out/bin/cpp"; ln -sf "$gcc" "$out/bin/gcc"
  for t in "$bmb"/out/bin/*; do ln -sf "$t" "$out/bin/`basename "$t"`"; done   # binutils-mesboot as/ld/ar
  ln -sf "$mk/make" "$out/bin/make"; ln -sf "$pd/patch" "$out/bin/patch"; ln -sf "$gawk/out/bin/gawk" "$out/bin/awk"; ln -sf "$gawk/out/bin/gawk" "$out/bin/gawk"
  for tool in sed:sed grep:grep cmp:diffutils diff:diffutils; do
    n=${tool%%:*}; pk=${tool##*:}; b=`command -v "$n" 2>/dev/null || ls /gnu/store/*$pk*/bin/$n 2>/dev/null | sort | head -1`
    test -n "$b" && ln -sf "$b" "$out/bin/$n" || true
  done
  ( cd "$src"
    # remove-bashism: make-syscalls.sh uses a bash ${var//./_} substitution
    sed -i 's,\${vdso_symver//\./_},$(echo $vdso_symver | sed -e "s/\\./_/g"),' sysdeps/unix/make-syscalls.sh 2>/dev/null || true
    # simplify-intl-tests: avoid the non-ASCII de.po (po2test.sed)
    sed -i 's,de\.po,en_GB.po,' catgets/Makefile intl/Makefile 2>/dev/null || true
    sed -i 's,/bin/pwd,pwd,' configure 2>/dev/null || true
    # td builds glibc 2.16.0 STATIC + library-only; the optional daemon/utility PROGRAMS that pull nss
    # (getpwnam &c.) can't link statically ("requires at runtime the shared libraries"). Drop them — the
    # nss CLIENT objects that go into libc are separate and unaffected. (Not needed by the toolchain.)
    sed -i '/^others *+= *nscd/d; /^others-pie *+= *nscd/d; /^install-sbin *:= *nscd/d' nscd/Makefile 2>/dev/null || true
    # drop the `manual` subdir (texinfo docs): its install builds libc.info via makeinfo, which chokes on
    # glibc 2.16.0's .texi (same class as the gcc-4.6.4 makeinfo issue). Not needed for the library.
    sed -i 's/wctype manual shadow/wctype shadow/' Makeconfig 2>/dev/null || true ) || true
  bp="$gm1/out/bin:$out/bin:$cpath"; csh=`PATH="$bp" command -v sh`
  # the loop sandbox has NO /bin/sh (the host dev harness does, so this only bites in-sandbox): glibc's
  # Makeconfig hardcodes `SHELL := /bin/sh` (used by every recursive make) and ~14 scripts (mkinstalldirs
  # …) shebang `#! /bin/sh`. Point both at the curated sh.
  ( cd "$src"
    sed -i "s,^SHELL := /bin/sh,SHELL := $csh," Makeconfig 2>/dev/null || true
    for f in `grep -rl '^#! */bin/sh' . 2>/dev/null`; do sed -i "1s,^#! *[^ ]*/bin/sh,#!$csh," "$f" 2>/dev/null || true; done ) || true
  cppflags="-I $src/nptl/sysdeps/pthread/bits -D BOOTSTRAP_GLIBC=1"; cflags="-L $src -L $gld/out/lib"
  # The glibc build's BUILD tools (sunrpc/cross-*, etc.) are compiled with a bare `gcc` (gcc-mesboot1
  # WITHOUT our CC flags) and run on the build machine against glibc-mesboot0; gcc-mesboot1 has no glibc
  # headers/libs on its default path, so give them via C_INCLUDE_PATH/LIBRARY_PATH (guix's gcc-mesboot1
  # search-paths do this). Target objects use -nostdinc + the new 2.16.0 headers, so this can't leak in.
  BTINC="$gld/out/include:$kh"; BTLIB="$gld/out/lib"
  # td builds glibc 2.16.0 STATIC (guix builds it shared). td's whole chain is static and glibc-mesboot0
  # is --disable-shared/--without-tls; building shared made the new libnsl.so link fall back to that old
  # static libc.a for sunrpc symbols -> "errno TLS mismatches non-TLS". --disable-shared drops the shared
  # libs (libnsl.so &c.) entirely, so there is no shared-lib link to leak the old libc into. Keep guix's
  # --disable-obsolete-rpc since the leak path is gone.
  CFG="--disable-shared --enable-static --disable-obsolete-rpc --host=i686-unknown-linux-gnu --enable-static-nss --with-pthread --without-cvs --without-gd --enable-add-ons=nptl libc_cv_predef_stack_protector=no"
  run_cfg() { # $1 build subdir, $2 headers include dir, $3 prefix
    ( cd "$src" && rm -rf "$1" && mkdir -p "$1" && cd "$1"
      env PATH="$bp" CONFIG_SHELL="$csh" SHELL="$csh" libc_cv_friendly_stddef=yes libc_cv_ssp=false \
          C_INCLUDE_PATH="$BTINC" LIBRARY_PATH="$BTLIB" \
          CPP="$gcc -E $cppflags" CC="$gcc $cppflags $cflags" LD=gcc \
          "$csh" ../configure --prefix="$3" --with-headers="$2" $CFG ) ; }
  # append SHELL to the generated Makefile (remove-sunrpc-ish) so recipes use the curated sh
  fixmk() { printf '\nSHELL := %s\n' "$csh" >> "$src/$1/Makefile"; }
  HD2="$out/hdr"; mkdir -p "$HD2"
  echo "  >> glibc stage A (bootstrap headers)" >&2
  run_cfg build-hdr "$kh" "$HD2" >"$out/cfgA.log" 2>&1 || { echo "glibc headers configure failed" >&2; cp "$out/cfgA.log" "$ROOT/.td-build-cache/_glibcmesboot-cfgA.log" 2>/dev/null||true; tail -20 "$out/cfgA.log" >&2; return 1; }
  fixmk build-hdr
  ( cd "$src/build-hdr" && env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$BTINC" LIBRARY_PATH="$BTLIB" "$mk/make" SHELL="$csh" install-bootstrap-headers=yes install-headers ) >"$out/hdrbuild.log" 2>&1 \
    || { echo "glibc install-headers failed" >&2; cp "$out/hdrbuild.log" "$ROOT/.td-build-cache/_glibcmesboot-hdr.log" 2>/dev/null||true; tail -25 "$out/hdrbuild.log" >&2; return 1; }
  cp -a "$kh"/. "$HD2/include/" 2>/dev/null || true
  echo "  >> glibc stage B (full nptl library)" >&2
  run_cfg build "$HD2/include" "$out/out" >"$out/cfgB.log" 2>&1 || { echo "glibc full configure failed" >&2; cp "$out/cfgB.log" "$ROOT/.td-build-cache/_glibcmesboot-cfgB.log" 2>/dev/null||true; tail -20 "$out/cfgB.log" >&2; return 1; }
  fixmk build
  ( cd "$src/build" && env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$BTINC" LIBRARY_PATH="$BTLIB" "$mk/make" SHELL="$csh" ) >"$out/build.log" 2>&1 \
    || { echo "glibc full make failed" >&2; cp "$out/build.log" "$ROOT/.td-build-cache/_glibcmesboot-build.log" 2>/dev/null||true; tail -30 "$out/build.log" >&2; return 1; }
  # --disable-shared doesn't generate soversions.mk, but `make install` lists it as a prerequisite;
  # an empty one satisfies it (no shared libs to version).
  : > "$src/build/soversions.mk"
  ( cd "$src/build" && env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$BTINC" LIBRARY_PATH="$BTLIB" "$mk/make" SHELL="$csh" install ) >"$out/install.log" 2>&1 \
    || { echo "glibc install failed" >&2; cp "$out/install.log" "$ROOT/.td-build-cache/_glibcmesboot-install.log" 2>/dev/null||true; tail -20 "$out/install.log" >&2; return 1; }
  cp -a "$kh"/. "$out/out/include/" 2>/dev/null || true
  test -s "$out/out/lib/libc.a" -o -s "$out/out/lib/libc.so" || { echo "no glibc libc produced" >&2; return 1; }
}
# build_glibc_mesboot_shared — GNU C Library 2.16.0 built SHARED (libc.so.6 + ld-linux.so.2),
# the runtime glibc the /td/store dynamic wrapper links against (the static glibc 2.16.0 above
# builds GCC 4.9.4; this shared one is what dynamic /td/store binaries load). Skips the nis subdir
# (guix's glibc-mesboot ships no libnsl.so — guix-as-oracle) and relocates the ld scripts to bare names.
build_glibc_mesboot_shared() {
  cpath=$1; gm1=$2; bmb=$3; gawk=$4; gld=$5; mk=$6; pd=$7; out=$8
  rm -rf "$out"; mkdir -p "$out/bin"
  src="$out/src"; mkdir -p "$src"; tar -xzf "$GLIBC216_TB" -C "$src" --strip-components=1 || { echo "glibc-2.16.0 unpack failed" >&2; return 1; }
  ( cd "$src" && env -i "$pd/patch" --force -p1 -i "$GLIBC216_P1" && env -i "$pd/patch" --force -p1 -i "$GLIBC216_P2" ) >"$out/patch.log" 2>&1 \
    || { echo "glibc-2.16.0 boot-patch apply failed" >&2; tail -8 "$out/patch.log" >&2; return 1; }
  gcc="$gm1/out/bin/gcc"
  kh="$out/kh"; mkdir -p "$kh"; tar -xzf "$KH_TB" -C "$kh" || { echo "kernel headers unpack failed" >&2; return 1; }
  ln -sf "$gm1"/out/bin/cpp "$out/bin/cpp"; ln -sf "$gcc" "$out/bin/gcc"
  for t in "$bmb"/out/bin/*; do ln -sf "$t" "$out/bin/`basename "$t"`"; done   # binutils-mesboot as/ld/ar
  ln -sf "$mk/make" "$out/bin/make"; ln -sf "$pd/patch" "$out/bin/patch"; ln -sf "$gawk/out/bin/gawk" "$out/bin/awk"; ln -sf "$gawk/out/bin/gawk" "$out/bin/gawk"
  for tool in sed:sed grep:grep cmp:diffutils diff:diffutils; do
    n=${tool%%:*}; pk=${tool##*:}; b=`command -v "$n" 2>/dev/null || ls /gnu/store/*$pk*/bin/$n 2>/dev/null | sort | head -1`
    test -n "$b" && ln -sf "$b" "$out/bin/$n" || true
  done
  ( cd "$src"
    # remove-bashism: make-syscalls.sh uses a bash ${var//./_} substitution
    sed -i 's,\${vdso_symver//\./_},$(echo $vdso_symver | sed -e "s/\\./_/g"),' sysdeps/unix/make-syscalls.sh 2>/dev/null || true
    # simplify-intl-tests: avoid the non-ASCII de.po (po2test.sed)
    sed -i 's,de\.po,en_GB.po,' catgets/Makefile intl/Makefile 2>/dev/null || true
    sed -i 's,/bin/pwd,pwd,' configure 2>/dev/null || true
    # td builds glibc 2.16.0 STATIC + library-only; the optional daemon/utility PROGRAMS that pull nss
    # (getpwnam &c.) can't link statically ("requires at runtime the shared libraries"). Drop them — the
    # nss CLIENT objects that go into libc are separate and unaffected. (Not needed by the toolchain.)
    sed -i '/^others *+= *nscd/d; /^others-pie *+= *nscd/d; /^install-sbin *:= *nscd/d' nscd/Makefile 2>/dev/null || true
    # SHARED build: skip the `nis` subdir entirely (libnsl.so + the nis-based libnss_compat/nis/nisplus).
    # guix-as-oracle (guix build glibc-mesboot) PROVES guix ships NO libnsl.so (its output has nss/pthread/
    # rt/crypt/… but not nis) — exactly because nis/libnsl.so is where the shared link pulls clnt_gen.o from
    # the non-TLS glibc-mesboot0 (the errno-TLS clash). A dynamic program needs none of nis. Empty nis's
    # extra-libs so no nis shared lib is built; nss/libnss_files.so (resolving xdecrypt from gld) still builds.
    sed -i 's/^extra-libs[[:space:]]*=.*/extra-libs =/; s/^extra-libs-others[[:space:]]*=.*/extra-libs-others =/' nis/Makefile 2>/dev/null || true
    # drop the `manual` subdir (texinfo docs): its install builds libc.info via makeinfo, which chokes on
    # glibc 2.16.0's .texi (same class as the gcc-4.6.4 makeinfo issue). Not needed for the library.
    sed -i 's/wctype manual shadow/wctype shadow/' Makeconfig 2>/dev/null || true ) || true
  bp="$gm1/out/bin:$out/bin:$cpath"; csh=`PATH="$bp" command -v sh`
  # the loop sandbox has NO /bin/sh (the host dev harness does, so this only bites in-sandbox): glibc's
  # Makeconfig hardcodes `SHELL := /bin/sh` (used by every recursive make) and ~14 scripts (mkinstalldirs
  # …) shebang `#! /bin/sh`. Point both at the curated sh.
  ( cd "$src"
    sed -i "s,^SHELL := /bin/sh,SHELL := $csh," Makeconfig 2>/dev/null || true
    for f in `grep -rl '^#! */bin/sh' . 2>/dev/null`; do sed -i "1s,^#! *[^ ]*/bin/sh,#!$csh," "$f" 2>/dev/null || true; done ) || true
  # SHARED build (nm-verified): with obsolete-rpc ENABLED (CFG below), the NEW libc.so exports clnt_gen, so
  # the shared libnsl.so resolves it from the explicitly-listed new libc.so (no errno-TLS clash). But the new
  # libc.so does NOT export the secure-RPC xdecrypt (xcrypt.os is built but unexported), which glibc 2.16's
  # nss_files/files-key.c needs — so KEEP -L $gld/out/lib on the path: glibc-mesboot0's libc.a DOES export
  # xdecrypt (matches guix, which keeps -L <glibc-mesboot0>). clnt_gen still comes from the new libc.so.
  cppflags="-I $src/nptl/sysdeps/pthread/bits -D BOOTSTRAP_GLIBC=1"; cflags="-L $src -L $gld/out/lib"
  # The glibc build's BUILD tools (sunrpc/cross-*, etc.) are compiled with a bare `gcc` (gcc-mesboot1
  # WITHOUT our CC flags) and run on the build machine against glibc-mesboot0; gcc-mesboot1 has no glibc
  # headers/libs on its default path, so give them via C_INCLUDE_PATH/LIBRARY_PATH (guix's gcc-mesboot1
  # search-paths do this). Target objects use -nostdinc + the new 2.16.0 headers, so this can't leak in.
  # build TOOLS need <rpc/types.h>; take it from the NEW glibc's OWN source tree ($src/sunrpc has rpc/*.h),
  # not the build libc — so a --disable-obsolete-rpc build libc (no installed rpc headers) still works, and
  # the headers match the glibc being built (Option C: the build libc is 2.16.0, which omits rpc headers).
  BTINC="$src/sunrpc:$gld/out/include:$kh"; BTLIB="$gld/out/lib"
  # td builds glibc 2.16.0 STATIC (guix builds it shared). td's whole chain is static and glibc-mesboot0
  # is --disable-shared/--without-tls; building shared made the new libnsl.so link fall back to that old
  # static libc.a for sunrpc symbols -> "errno TLS mismatches non-TLS". --disable-shared drops the shared
  # libs (libnsl.so &c.) entirely, so there is no shared-lib link to leak the old libc into. Keep guix's
  # --disable-obsolete-rpc since the leak path is gone.
  # SHARED build: do NOT --disable-obsolete-rpc — glibc 2.16's nss_files/files-key.c references xdecrypt
  # (secure-RPC crypt), which --disable-obsolete-rpc omits, so the shared libnss_files.so won't link. The
  # obsolete RPC is enabled by default in 2.16; building it defines xdecrypt. Also drop --enable-static-nss
  # (it forces static NSS, pointless/awkward for a shared libc). guix's shared glibc-mesboot builds full RPC.
  # guix's EXACT glibc-mesboot flags (commencement.scm glibc-headers-mesboot, inherited): --disable-obsolete-rpc
  # + --enable-static-nss. The boot patch un-hides the sunrpc symbols (libc_hidden_nolink_sunrpc → empty) so
  # libc.so EXPORTS xdecrypt/clnt_gen even with --disable-obsolete-rpc — which is why guix needs no special
  # -L handling. Pairs with the SINGLE-stage build above (the two-stage variant defeated the un-hiding).
  CFG="--enable-shared --disable-obsolete-rpc --host=i686-unknown-linux-gnu --enable-static-nss --with-pthread --without-cvs --without-gd --enable-add-ons=nptl libc_cv_predef_stack_protector=no"
  run_cfg() { # $1 build subdir, $2 headers include dir, $3 prefix
    ( cd "$src" && rm -rf "$1" && mkdir -p "$1" && cd "$1"
      env PATH="$bp" CONFIG_SHELL="$csh" SHELL="$csh" libc_cv_friendly_stddef=yes libc_cv_ssp=false \
          C_INCLUDE_PATH="$BTINC" LIBRARY_PATH="$BTLIB" \
          CPP="$gcc -E $cppflags" CC="$gcc $cppflags $cflags" LD=gcc \
          "$csh" ../configure --prefix="$3" --with-headers="$2" $CFG ) ; }
  # append SHELL to the generated Makefile (remove-sunrpc-ish) so recipes use the curated sh
  fixmk() { printf '\nSHELL := %s\n' "$csh" >> "$src/$1/Makefile"; }
  # SINGLE-stage build, matching guix's glibc-mesboot phases (NOT the two-stage bootstrap-headers→full I
  # used for the static rung): ONE configure with --with-headers=<kernel headers>, then make + make install.
  # The two-stage variant defeated the boot patch's sunrpc un-hiding (the new libc.so didn't export
  # xdecrypt/clnt_gen), forcing the -L gld pull + errno clash; the single configure is what guix does.
  echo "  >> glibc (single-stage, full nptl library) — guix's glibc-mesboot phases" >&2
  run_cfg build "$kh" "$out/out" >"$out/cfgB.log" 2>&1 || { echo "glibc configure failed" >&2; cp "$out/cfgB.log" "$ROOT/.td-build-cache/_glibcmesboot-cfgB.log" 2>/dev/null||true; tail -20 "$out/cfgB.log" >&2; return 1; }
  fixmk build
  ( cd "$src/build" && env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$BTINC" LIBRARY_PATH="$BTLIB" "$mk/make" SHELL="$csh" ) >"$out/build.log" 2>&1 \
    || { echo "glibc full make failed" >&2; cp "$out/build.log" "$ROOT/.td-build-cache/_glibcmesboot-build.log" 2>/dev/null||true; tail -30 "$out/build.log" >&2; return 1; }
  # shared build: the real soversions.mk is generated by the build — do NOT stub it (would clobber
  # the shared-lib versions). (static variant stubbed an empty one because --disable-shared omits it.)
  true
  ( cd "$src/build" && env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= C_INCLUDE_PATH="$BTINC" LIBRARY_PATH="$BTLIB" "$mk/make" SHELL="$csh" install ) >"$out/install.log" 2>&1 \
    || { echo "glibc install failed" >&2; cp "$out/install.log" "$ROOT/.td-build-cache/_glibcmesboot-install.log" 2>/dev/null||true; tail -20 "$out/install.log" >&2; return 1; }
  cp -a "$kh"/. "$out/out/include/" 2>/dev/null || true
  test -s "$out/out/lib/libc.a" -o -s "$out/out/lib/libc.so" || { echo "no glibc libc produced" >&2; return 1; }
}

# build_gcc_mesboot — GCC 4.9.4 (guix's gcc-mesboot), the FINAL mesboot gcc. Built by gcc-mesboot1
# (4.6.4 C/C++) + binutils-mesboot + glibc-mesboot 2.16.0, with gmp-4.3.2/mpfr-2.4.2/mpc-1.0.3 unpacked
# in-tree. Single gcc-4.9.4 tarball (no modular g++ download, no boot patch — guix deletes both phases).
# Same static divergence as the earlier gcc rungs: td's glibc-mesboot is static-only, so we build
# --disable-shared --enable-static (guix builds --enable-shared against its shared glibc, via the
# gcc-mesboot1-wrapper's dynamic-linker; the static build needs neither). guix's gcc-mesboot env:
# C_INCLUDE_PATH = <gm1>/lib/gcc/.../4.6.4/include : <kernel-headers> : <glibc>/include : <cwd>/mpfr/src;
# CPLUS_INCLUDE_PATH mirrors it; LIBRARY_PATH = <glibc>/lib : <gm1>/lib.
build_gcc_mesboot() {
  cpath=$1; gm1=$2; bmb=$3; glibc=$4; mm=$5; pd=$6; out=$7
  rm -rf "$out"; mkdir -p "$out/bin"
  # gcc-4.9.4 is a .tar.bz2 (guix's pin); the loop sandbox has no bzip2 on PATH (tar's -j can't exec it),
  # so decompress with a store bzip2 (build scaffolding, like the awk/flex globs) piped to plain tar.
  bz=`command -v bzip2 2>/dev/null || ls /gnu/store/*-bzip2-*/bin/bzip2 2>/dev/null | sort | head -1`
  test -n "$bz" || { echo "no bzip2 to decompress gcc-4.9.4" >&2; return 1; }
  "$bz" -dc "$GCC494_TB" | tar -xf - -C "$out" --strip-components=1 || { echo "gcc-4.9.4 unpack failed" >&2; return 1; }
  tar -xzf "$GMP_TB" -C "$out" && tar -xzf "$MPFR_TB" -C "$out" && tar -xzf "$MPC_TB" -C "$out" \
    || { echo "gmp/mpfr/mpc unpack failed" >&2; return 1; }
  ( cd "$out" && ln -sf gmp-4.3.2 gmp && ln -sf mpfr-2.4.2 mpfr && ln -sf mpc-1.0.3 mpc ) \
    || { echo "gmp/mpfr/mpc symlink failed" >&2; return 1; }
  gcc="$gm1/out/bin/gcc"; gm1inc="$gm1/out/lib/gcc/i686-unknown-linux-gnu/4.6.4/include"
  kh="$out/kh"; mkdir -p "$kh"; tar -xzf "$KH_TB" -C "$kh" || { echo "kernel headers unpack failed" >&2; return 1; }
  ln -sf "$gm1"/out/bin/cpp "$out/bin/cpp"
  for t in "$bmb"/out/bin/*; do ln -sf "$t" "$out/bin/`basename "$t"`"; done   # binutils-mesboot as/ld/ar
  ln -sf "$mm/make" "$out/bin/make"; ln -sf "$pd/patch" "$out/bin/patch"
  awkb=`command -v awk 2>/dev/null || ls /gnu/store/*gawk*/bin/awk 2>/dev/null | sort | head -1`
  flexb=`command -v flex 2>/dev/null || ls /gnu/store/*flex*/bin/flex 2>/dev/null | sort | head -1`
  bisonb=`command -v bison 2>/dev/null || ls /gnu/store/*bison*/bin/bison 2>/dev/null | sort | head -1`
  cmpb=`command -v cmp 2>/dev/null || ls /gnu/store/*diffutils*/bin/cmp 2>/dev/null | sort | head -1`
  diffb=`command -v diff 2>/dev/null || ls /gnu/store/*diffutils*/bin/diff 2>/dev/null | sort | head -1`
  test -n "$awkb" -a -n "$flexb" -a -n "$bisonb" -a -n "$cmpb" -a -n "$diffb" || { echo "need awk/flex/bison/cmp/diff (build tools) from the store" >&2; return 1; }
  ln -sf "$awkb" "$out/bin/awk"; ln -sf "$flexb" "$out/bin/flex"; ln -sf "$flexb" "$out/bin/lex"
  ln -sf "$bisonb" "$out/bin/bison"; ln -sf "$bisonb" "$out/bin/yacc"
  ln -sf "$cmpb" "$out/bin/cmp"; ln -sf "$diffb" "$out/bin/diff"
  CIP="$gm1inc:$kh:$glibc/out/include:$out/mpfr/src"; LP="$glibc/out/lib:$gm1/out/lib"
  ldf="-static -B$glibc/out/lib"
  ( cd "$out"; bp="$gm1/out/bin:$out/bin:$cpath"; csh=`PATH="$bp" command -v sh`
    # sandbox has no /bin/sh: rewrite script shebangs to the curated sh (kernel execs the shebang path)
    for f in `grep -rl '^#! */bin/sh' . 2>/dev/null`; do sed -i "1s,^#! *[^ ]*/bin/sh,#!$csh," "$f" 2>/dev/null || true; done
    rm -rf bld; mkdir bld; cd bld
    # gcc-mesboot1's (4.6.4) libgcc unwinder references dl_iterate_phdr; a default link can't resolve it
    # against td's static-only glibc-mesboot (guix links dynamically via the wrapper + shared glibc). So the
    # configure link test must be STATIC too — pass LDFLAGS=-static -B<glibc>/lib so conftest pulls libc.a.
    # CC stays CLEAN (no -static/-B): those pollute autoconf's compile/preprocess header tests (the "-B
    # never used" warning made HAVE_LIMITS_H=no on an earlier rung). LDFLAGS (link-only) makes the HOST
    # link test static. CC_FOR_BUILD compiles+RUNS build tools (gmp gen-*), so it must link static to run
    # against the static-only glibc — `$gcc -static` (gm1's default glibc-mesboot0 crt, which has
    # dl_iterate_phdr; no -B so no header-test warning).
    env PATH="$bp" CONFIG_SHELL="$csh" CC="$gcc" CPP="$gcc -E" CC_FOR_BUILD="$gcc -static" C_INCLUDE_PATH="$CIP" CPLUS_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" LDFLAGS="$ldf" \
        "$csh" ../configure --prefix="$out/out" --build=i686-unknown-linux-gnu --host=i686-unknown-linux-gnu \
        --with-host-libstdcxx=-lsupc++ \
        --with-native-system-header-dir="$glibc/out/include" --with-build-sysroot="$glibc/out/include" \
        --disable-bootstrap --disable-decimal-float --disable-libatomic --disable-libcilkrts --disable-libgomp \
        --disable-libitm --disable-libmudflap --disable-libquadmath --disable-libsanitizer --disable-libssp \
        --disable-libvtv --disable-lto --disable-lto-plugin --disable-multilib --disable-plugin \
        --enable-languages=c,c++ --enable-static --disable-shared --enable-threads=single --disable-libstdcxx-pch \
        --disable-build-with-cxx >cfg.log 2>&1 \
      || { echo "gcc-mesboot configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_gccmesboot-cfg.log" 2>/dev/null||true; tail -25 cfg.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" CC_FOR_BUILD="$gcc -static" C_INCLUDE_PATH="$CIP" CPLUS_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mm/make" SHELL="$csh" CONFIG_SHELL="$csh" CC_FOR_BUILD="$gcc -static" MAKEINFO=true "LDFLAGS=$ldf" "LDFLAGS_FOR_TARGET=$ldf" >build.log 2>&1 \
      || { echo "gcc-mesboot make failed" >&2; cp build.log "$ROOT/.td-build-cache/_gccmesboot-build.log" 2>/dev/null||true; tail -40 build.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" C_INCLUDE_PATH="$CIP" CPLUS_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        "$mm/make" SHELL="$csh" MAKEINFO=true install >install.log 2>&1 \
      || { echo "gcc-mesboot install failed" >&2; cp install.log "$ROOT/.td-build-cache/_gccmesboot-install.log" 2>/dev/null||true; tail -20 install.log >&2; return 1; }
  ) || return 1
  test -x "$out/out/bin/gcc" -a -x "$out/out/bin/g++" || { echo "no gcc/g++ produced" >&2; return 1; }
}
build_gcc_14() {
  cpath=$1; gccm=$2; glibc=$3; bmb=$4; out=$5
  rm -rf "$out"; mkdir -p "$out/bin"
  gcc="$gccm/bin/gcc"; gpp="$gccm/bin/g++"; g494inc="$gccm/lib/gcc/i686-unknown-linux-gnu/4.9.4/include"
  xzb=`command -v xz 2>/dev/null || ls /gnu/store/*'xz-'*/bin/xz 2>/dev/null | sort | head -1`
  test -n "$xzb" || { echo "no xz to unpack gcc-14.3.0" >&2; return 1; }
  "$xzb" -dc "$GCC14_TB" | tar -xf - -C "$out" --strip-components=1 || { echo "gcc-14.3.0 unpack failed" >&2; return 1; }
  "$xzb" -dc "$GMP63_TB" | tar -xf - -C "$out" || { echo "gmp unpack failed" >&2; return 1; }
  "$xzb" -dc "$MPFR421_TB" | tar -xf - -C "$out" || { echo "mpfr unpack failed" >&2; return 1; }
  tar -xzf "$MPC131_TB" -C "$out" || { echo "mpc unpack failed" >&2; return 1; }
  ( cd "$out" && ln -sf gmp-6.3.0 gmp && ln -sf mpfr-4.2.1 mpfr && ln -sf mpc-1.3.1 mpc ) || { echo "gmp/mpfr/mpc symlink failed" >&2; return 1; }
  kh="$out/kh"; mkdir -p "$kh"; tar -xzf "$KH_TB" -C "$kh" || { echo "kernel headers unpack failed" >&2; return 1; }
  ln -sf "$gccm"/bin/cpp "$out/bin/cpp"
  for t in "$bmb"/bin/*; do ln -sf "$t" "$out/bin/`basename "$t"`"; done
  for tool in awk:gawk flex:flex bison:bison cmp:diffutils diff:diffutils sed:sed grep:grep m4:m4 make:make; do
    n=${tool%%:*}; pk=${tool##*:}; b=`command -v "$n" 2>/dev/null || ls /gnu/store/*$pk*/bin/$n 2>/dev/null | sort | head -1`
    test -n "$b" && ln -sf "$b" "$out/bin/$n" || true
  done
  ln -sf "$out/bin/flex" "$out/bin/lex" 2>/dev/null||true; ln -sf "$out/bin/bison" "$out/bin/yacc" 2>/dev/null||true
  csh=`PATH="$out/bin:$cpath" command -v sh`
  mkdir -p "$out/wb"
  printf '#!%s\nexec "%s" -static -B%s/lib "$@"\n' "$csh" "$gcc" "$glibc" > "$out/wb/gcc"
  printf '#!%s\nexec "%s" -static -B%s/lib "$@"\n' "$csh" "$gpp" "$glibc" > "$out/wb/g++"
  chmod 0555 "$out/wb/gcc" "$out/wb/g++"
  CIP="$g494inc:$kh:$glibc/include:$out/mpfr/src"; LP="$glibc/lib:$gccm/lib"; ldf="-static -B$glibc/lib"
  ( cd "$out"; bp="$gccm/bin:$out/bin:$cpath"
    for f in `grep -rl '^#! */bin/sh' . 2>/dev/null`; do sed -i "1s,^#! *[^ ]*/bin/sh,#!$csh," "$f" 2>/dev/null || true; done
    rm -rf bld; mkdir bld; cd bld
    env PATH="$bp" CONFIG_SHELL="$csh" CC="$out/wb/gcc" CXX="$out/wb/g++" CPP="$out/wb/gcc -E" \
        CC_FOR_BUILD="$out/wb/gcc" CXX_FOR_BUILD="$out/wb/g++" \
        C_INCLUDE_PATH="$CIP" CPLUS_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" LDFLAGS="$ldf" \
        "$csh" ../configure --prefix=/td/store/gcc-14.3.0 \
        --build=i686-unknown-linux-gnu --host=i686-unknown-linux-gnu \
        --with-native-system-header-dir=/include --with-build-sysroot="$glibc" \
        --disable-bootstrap --disable-multilib --disable-shared --enable-static \
        --enable-languages=c,c++ --enable-threads=single --disable-libstdcxx-pch \
        --disable-libatomic --disable-libgomp --disable-libitm --disable-libsanitizer \
        --disable-libssp --disable-libvtv --disable-libquadmath --disable-lto --disable-plugin \
        --disable-decimal-float --disable-werror >cfg.log 2>&1 \
      || { echo "gcc-14.3.0 configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_gcc1430-cfg.log" 2>/dev/null||true; tail -25 cfg.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" \
        C_INCLUDE_PATH="$CIP" CPLUS_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        make -j"$BJOBS" SHELL="$csh" CONFIG_SHELL="$csh" MAKEINFO=true "LDFLAGS=$ldf" "LDFLAGS_FOR_TARGET=$ldf" >build.log 2>&1 \
      || { echo "gcc-14.3.0 make failed" >&2; cp build.log "$ROOT/.td-build-cache/_gcc1430-build.log" 2>/dev/null||true; tail -40 build.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" \
        C_INCLUDE_PATH="$CIP" CPLUS_INCLUDE_PATH="$CIP" LIBRARY_PATH="$LP" \
        make SHELL="$csh" MAKEINFO=true install DESTDIR="$out/stage" >install.log 2>&1 \
      || { echo "gcc-14.3.0 install failed" >&2; tail -20 install.log >&2; return 1; } ) || return 1
  test -x "$out/stage/td/store/gcc-14.3.0/bin/gcc" -a -x "$out/stage/td/store/gcc-14.3.0/bin/g++" || { echo "no gcc/g++ 14.3.0 produced" >&2; return 1; }
}

# build_binutils_244 — MODERN GNU Binutils 2.44 built SANDBOX-RUNNABLE (build-dir glibc 2.16.0 interp, so its
# as/ld run during the sandbox glibc build) by gcc-mesboot1 (4.6.4) — glibc 2.41 needs a modern binutils
# (2.20.1a is "too old"). Same build as the binutils-244 gate but CC bakes the LIVE build-dir interp, not
# /td/store. -std=gnu99 (binutils 2.44 is C99+; gcc 4.6.4 default gnu89), cross-style, --disable-gold, MAKEINFO=true.
build_binutils_244() {
  cpath=$1; gm1=$2; gls=$3; bmb=$4; out=$5
  rm -rf "$out"; mkdir -p "$out"
  gm1dir="$gm1/lib/gcc/i686-unknown-linux-gnu/4.6.4"
  xzb=`command -v xz 2>/dev/null || ls /gnu/store/*'xz-'*/bin/xz 2>/dev/null | sort | head -1`
  test -n "$xzb" || { echo "no xz" >&2; return 1; }
  sh=`command -v bash 2>/dev/null || command -v sh`
  tb=`mktemp -d`/tb; mkdir -p "$tb"
  for tool in awk:gawk flex:flex bison:bison cmp:diffutils diff:diffutils; do
    n=${tool%%:*}; pk=${tool##*:}; b=`command -v "$n" 2>/dev/null || ls /gnu/store/*$pk*/bin/$n 2>/dev/null | sort | head -1`
    test -n "$b" && ln -sf "$b" "$tb/$n" || true; done
  ln -sf "$tb/flex" "$tb/lex" 2>/dev/null||true; ln -sf "$tb/bison" "$tb/yacc" 2>/dev/null||true
  wb=`mktemp -d`/wb; mkdir -p "$wb"
  printf '#!%s\nexec "%s/bin/gcc" -std=gnu99 -isystem "%s/include" -B"%s/lib" -L"%s/lib" -L"%s" -Wl,--dynamic-linker -Wl,%s/lib/ld-linux.so.2 -Wl,-rpath -Wl,%s/lib "$@"\n' \
    "$sh" "$gm1" "$gls" "$gls" "$gls" "$gm1dir" "$gls" "$gls" > "$wb/gcc"
  chmod 0555 "$wb/gcc"
  src=`mktemp -d`/binutils; mkdir -p "$src"
  "$xzb" -dc "$BU244_TB" | tar -xf - -C "$src" --strip-components=1 || { echo "binutils-2.44 unpack failed" >&2; return 1; }
  ( cd "$src"; bp="$bmb/bin:$tb:$cpath"
    env PATH="$bp" CONFIG_SHELL="$sh" SHELL="$sh" CC="$wb/gcc" CC_FOR_BUILD="$wb/gcc" AR="$bmb/bin/ar" RANLIB="$bmb/bin/ranlib" \
      "$sh" ./configure --build=i686-pc-linux-gnu --host=i686-unknown-linux-gnu --prefix=/td/store/binutils-2.44 \
      --disable-nls --disable-gold --disable-werror --enable-deterministic-archives --disable-plugins --disable-gprofng >cfg.log 2>&1 \
      || { echo "binutils-2.44 configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_bu244sb-cfg.log" 2>/dev/null||true; tail -25 cfg.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$sh" SHELL="$sh" make -j"$BJOBS" MAKEINFO=true >build.log 2>&1 \
      || { echo "binutils-2.44 make failed" >&2; cp build.log "$ROOT/.td-build-cache/_bu244sb-build.log" 2>/dev/null||true; tail -30 build.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$sh" SHELL="$sh" make MAKEINFO=true install prefix="$out" >inst.log 2>&1 \
      || { echo "binutils-2.44 install failed" >&2; tail -20 inst.log >&2; return 1; } ) || return 1
  test -x "$out/bin/as" -a -x "$out/bin/ld" || { echo "no as/ld produced" >&2; return 1; }
}
# build_glibc_241 — MODERN glibc 2.41 (guix's glibc-final) built by gcc 14.3.0 + binutils 2.44 against the
# linux kernel headers — a SHARED libc. CC bakes only the build-dir glibc 2.16.0 INTERP (NO -rpath: glibc 2.41
# forbids DT_RPATH AND DT_RUNPATH in libc.so.6); the build tools find glibc 2.16.0 via LD_LIBRARY_PATH. Install
# via DESTDIR (prefix /td/store/glibc-2.41 is unwritable).
build_glibc_241() {
  cpath=$1; gcc14=$2; gls=$3; bu244=$4; out=$5
  rm -rf "$out"; mkdir -p "$out"
  xzb=`command -v xz 2>/dev/null || ls /gnu/store/*'xz-'*/bin/xz 2>/dev/null | sort | head -1`
  csh=`command -v bash 2>/dev/null || command -v sh`
  kh=`mktemp -d`/kh; mkdir -p "$kh"; tar -xzf "$KH_TB" -C "$kh" || { echo "kernel headers unpack failed" >&2; return 1; }
  src=`mktemp -d`/glibc; mkdir -p "$src/bin"
  "$xzb" -dc "$GLIBC241_TB" | tar -xf - -C "$src" --strip-components=1 || { echo "glibc-2.41 unpack failed" >&2; return 1; }
  for t in "$bu244"/bin/*; do ln -sf "$t" "$src/bin/`basename "$t"`"; done
  for tool in awk:gawk gawk:gawk sed:sed grep:grep make:make m4:m4 bison:bison flex:flex msgfmt:gettext makeinfo:texinfo python3:python gzip:gzip; do
    n=${tool%%:*}; pk=${tool##*:}; b=`command -v "$n" 2>/dev/null || ls /gnu/store/*$pk*/bin/$n 2>/dev/null | sort | head -1`
    test -n "$b" && ln -sf "$b" "$src/bin/$n" || true; done
  wb=`mktemp -d`/wb; mkdir -p "$wb"
  printf '#!%s\nexec "%s/bin/gcc" -B%s/lib -L%s/lib -isystem %s/include -static-libgcc -Wl,--dynamic-linker -Wl,%s/lib/ld-linux.so.2 "$@"\n' \
    "$csh" "$gcc14" "$gls" "$gls" "$gls" "$gls" > "$wb/gcc"
  chmod 0555 "$wb/gcc"
  ( cd "$src"
    for f in `grep -rl '^#! */bin/sh' . 2>/dev/null`; do sed -i "1s,^#! *[^ ]*/bin/sh,#!$csh," "$f" 2>/dev/null || true; done
    sed -i "s,^SHELL := /bin/sh,SHELL := $csh," Makeconfig 2>/dev/null || true
    rm -rf bld; mkdir bld; cd bld
    env PATH="$src/bin:$cpath" CONFIG_SHELL="$csh" SHELL="$csh" CC="$wb/gcc" LD_LIBRARY_PATH="$gls/lib" \
      "$csh" ../configure --prefix=/td/store/glibc-2.41 --build=i686-pc-linux-gnu --host=i686-unknown-linux-gnu \
      --with-headers="$kh" --enable-kernel=3.2.0 --disable-werror --disable-nscd --with-binutils="$bu244/bin" \
      libc_cv_slibdir=/td/store/glibc-2.41/lib >cfg.log 2>&1 \
      || { echo "glibc-2.41 configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_glibc241-cfg.log" 2>/dev/null||true; tail -30 cfg.log >&2; return 1; }
    env PATH="$src/bin:$cpath" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" SHELL="$csh" LD_LIBRARY_PATH="$gls/lib" make -j"$BJOBS" >build.log 2>&1 \
      || { echo "glibc-2.41 make failed" >&2; cp build.log "$ROOT/.td-build-cache/_glibc241-build.log" 2>/dev/null||true; tail -40 build.log >&2; return 1; }
    env PATH="$src/bin:$cpath" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= CONFIG_SHELL="$csh" SHELL="$csh" LD_LIBRARY_PATH="$gls/lib" make install DESTDIR="$out/stage" >inst.log 2>&1 \
      || { echo "glibc-2.41 install failed" >&2; tail -20 inst.log >&2; return 1; } ) || return 1
  test -e "$out/stage/td/store/glibc-2.41/lib/libc.so.6" -a -e "$out/stage/td/store/glibc-2.41/lib/ld-linux.so.2" || { echo "no libc.so.6/ld.so produced" >&2; return 1; }
}

# bootstrap_modern_toolchain — verify the pinned inputs, then build the full chain from the seed and
# finalize the modern toolchain (no /gnu/store, relocated glibc ld-scripts, kernel headers in glibc).
bootstrap_modern_toolchain() {
# brick8's elf-set-interp rewrites PT_INTERP IN PLACE (shrink-or-equal; td's elf.rs has no patchelf-style
# grow). The toolchain binaries' build-time interp lives under TMPDIR (…/glibcsharedbuild/out/lib/ld-linux.so.2)
# and MUST be >= the /td/store target interp (/td/store/<32-char-hash>-glibc-2.41/lib/ld-linux.so.2, 71 chars),
# or the rewrite silently no-ops (|| true) and ld/as keep a build-dir interp that does NOT exist in
# build-recipe's /td/store-only pivot sandbox → "C compiler cannot create executables". check.sh's default
# TMPDIR=/tmp is too short (build interp ~58 < 71); pin a deliberately-long TMPDIR under the worktree's
# .td-build-cache and HARD-ASSERT it stays long enough (so this can never silently regress).
TMPDIR="$ROOT/.td-build-cache/chain-build-tmp-keep-interp-paths-long-for-elf-set-interp"; mkdir -p "$TMPDIR"; export TMPDIR
_ip="$TMPDIR/tmp.XXXXXXXX/glibcsharedbuild/out/lib/ld-linux.so.2"
test ${#_ip} -ge 75 || fail "TMPDIR too short ($TMPDIR): build-dir interp ${#_ip}<75 chars would break elf-set-interp's in-place PT_INTERP rewrite to the /td/store glibc (71 chars)"
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
# the host-produced kernel-headers tarball (td-feed warm sources; derived from the pinned linux src)
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

# build_gcc_14 — MODERN GCC 14.3.0 (guix's gcc-boot0/gcc-final version) built by gcc-mesboot 4.9.4 against
# the static glibc 2.16.0, with gmp-6.3.0/mpfr-4.2.1/mpc-1.3.1 (gcc 14's contrib/download_prerequisites
# versions) unpacked in-tree. Built STATIC (like 4.9.4) so gcc 14's own xgcc/cc1 run in the sandbox. CC is a
# -static wrapper SCRIPT (gcc derives CC_FOR_BUILD from CC on a native build and strips trailing flags from a
# plain CC_FOR_BUILD, so the in-tree gmp 6.3.0's build tools would come out dynamic and fail to run — a
# single-token wrapper survives the munging). The sysroot is passed as --with-build-sysroot=<glibc> +
# --with-native-system-header-dir=/include (gcc 14 CONCATENATES the two; both absolute → a doubled header
# path that breaks fixincludes). MAKEINFO=true. Installs via DESTDIR (prefix is the unwritable /td/store).
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
GLIBC241B=`mktemp -d`/glibc241build; build_glibc_241 "$cpath" "$GCC14B/stage/td/store/gcc-14.3.0" "$GSH/out" "$BMB244SB" "$GLIBC241B" || fail "the toolchain did not build the modern glibc 2.41"
trap 'rm -rf "$tc" "$mesp" "`dirname "$TCCD"`" "`dirname "$MK"`" "`dirname "$PD"`" "`dirname "$BD"`" "`dirname "$GD"`" "`dirname "$HD"`" "`dirname "$GLD"`" "`dirname "$G2"`" "`dirname "$B2"`" "`dirname "$MM"`" "`dirname "$GM1"`" "`dirname "$BMB"`" "`dirname "$GAWKMB"`" "`dirname "$GOUT"`" "`dirname "$GMB"`" "`dirname "$GSH"`" "`dirname "$GCC14B"`" "`dirname "$BMB244SB"`" "`dirname "$GLIBC241B"`" "`dirname "$cpath"`"' EXIT INT TERM

GCC14="$GCC14B/stage/td/store/gcc-14.3.0"
GLIBC241="$GLIBC241B/stage/td/store/glibc-2.41"
CC1=`ls "$GCC14"/libexec/gcc/i686-unknown-linux-gnu/14.3.0/cc1 2>/dev/null || true`

# --- [no-guix] the modern glibc 2.41 + gcc 14 carry no guix bytes -----------------------------------
test -e "$GLIBC241/lib/libc.so.6" -a -e "$GLIBC241/lib/ld-linux.so.2" || fail "glibc 2.41 missing libc.so.6/ld-linux.so.2"
for b in "$GLIBC241/lib/libc.so.6" "$GCC14/bin/gcc" "$CC1"; do
  test -n "$b" -a -e "$b" || fail "output missing ($b)"
  if grep -q -a '/gnu/store' "$b"; then fail "$b contains /gnu/store bytes"; fi
done
echo "   [no-guix] seed → … → gcc 14.3.0 + binutils 2.44 → MODERN glibc 2.41; no /gnu/store in libc.so.6 / gcc / cc1"
# relocate glibc 2.41's ld scripts: strip the configure PREFIX path to bare names (ld finds them via -L).
for so in "$GLIBC241/lib/"*.so; do
  if head -c20 "$so" 2>/dev/null | grep -q 'GNU ld script' 2>/dev/null; then sed -i "s,/td/store/glibc-2.41/lib/,,g; s,$GLIBC241/lib/,,g" "$so"; fi
done
# BRICK 8: glibc's headers `#include <linux/*>`/`<asm/*>` (kernel UAPI) — add the SAME pure kernel headers the
# glibc build used into glibc 2.41's include dir so a --sysroot corpus build finds them (else bits/local_lim.h
# → "linux/limits.h: No such file"). Harmless for the C/C++ verify above (it includes no kernel headers).
khb8=`mktemp -d`; tar -xzf "$KH_TB" -C "$khb8" || fail "brick8: kernel headers unpack failed"
for kd in linux asm asm-generic mtd rdma scsi sound video xen drm misc; do test -d "$khb8/$kd" && cp -rn "$khb8/$kd" "$GLIBC241/include/" 2>/dev/null || true; done
test -e "$GLIBC241/include/linux/limits.h" || fail "brick8: kernel headers not added to glibc include"
}
