#!/bin/sh
# tests/bootstrap-binutils.sh — source-bootstrap BRICK 5 (gcc toolchain): the seed-built tcc + the
# tcc-built GNU Make build GNU Binutils 2.20.1a to produce `as` + `ld` — the first real assembler and
# linker in the /td/store toolchain, which the gcc-mesboot rungs need. Exactly guix's binutils-mesboot0.
#
# From the 229-byte seed, td builds Mes + MesCC + tcc + make + patch (bricks 0-4 + the make/patch
# rungs), then the td-built `patch` applies guix's binutils boot patch (vendored byte-for-byte at
# seed/patches/binutils-boot-2.20.1a.patch — it makes 2.20.1a build under tcc + Mes libc: drops
# C99isms, fixes the malloc proto, adds includes), and the tcc-built make drives tcc over the
# patched tree. This is the first RECURSIVE-make build in the chain (bfd/gas/ld/binutils/…), so it
# leans on the make-in-sandbox fixes the patch rung found (SHELL make-variable override + clearing
# the inherited MAKEFLAGS/jobserver of the loop's outer `make -j2`).
#
# i686, static. Sources (mes + nyacc + tcc + make + patch + binutils) are td-fetched, not vendored;
# only the guix boot patch is vendored (it is source data, not a guix-built byte — the output `as`/`ld`
# are compiled from binutils source + the patch). Serial, like guix #:parallel-build? #f.
#
# Legs (DURABLE):
#   [pinned-input] mes + nyacc + tcc + make + patch + binutils tarballs match their lock sha256;
#                  the vendored boot patch matches its recorded sha256.
#   [no-guix]      built on a curated PATH with gcc/g++/cc/guile/guix DENIED; no /gnu/store in as/ld.
#   [behavioral]   as + ld run and report 2.20.1, and together assemble+link a tiny i386 program that
#                  RUNS and returns 42 — a real assembler+linker built by the seed toolchain.
#   [repro]        two independent binutils builds (same dir) yield byte-identical as + ld.
set -eu

ROOT=$(pwd)
fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
STAGE0=seed/stage0
A=AMD64
BOOT_PATCH="$ROOT/seed/patches/binutils-boot-2.20.1a.patch"
BOOT_PATCH_SHA=f6be78a06f2c9905e019ade08f701e5468386cf1934aa27757a64c619571da20

# --- [pinned-input] all source tarballs + the vendored boot patch match their pins ----------------
lf() { sed -n "s/^$2 //p" "$1" | head -1; }
MES_LOCK=`ls seed/sources/mes-*.lock | head -1`;       NYACC_LOCK=`ls seed/sources/nyacc-*.lock | head -1`
TCC_LOCK=`ls seed/sources/tcc-0.9.26*.lock | head -1`; MAKE_LOCK=`ls seed/sources/make-*.lock | head -1`
PATCH_LOCK=`ls seed/sources/patch-*.lock | head -1`;   BU_LOCK=`ls seed/sources/binutils-*.lock | head -1`
for l in "$MES_LOCK" "$NYACC_LOCK" "$TCC_LOCK" "$MAKE_LOCK" "$PATCH_LOCK" "$BU_LOCK"; do test -n "$l" || fail "missing a seed/sources/*.lock"; done
MES_TB=".td-build-cache/sources/`lf "$MES_LOCK" file`";     NYACC_TB=".td-build-cache/sources/`lf "$NYACC_LOCK" file`"
TCC_TB=".td-build-cache/sources/`lf "$TCC_LOCK" file`";     MAKE_TB=".td-build-cache/sources/`lf "$MAKE_LOCK" file`"
PATCH_TB=".td-build-cache/sources/`lf "$PATCH_LOCK" file`"; BU_TB=".td-build-cache/sources/`lf "$BU_LOCK" file`"
for pair in "$MES_TB:`lf "$MES_LOCK" sha256`" "$NYACC_TB:`lf "$NYACC_LOCK" sha256`" "$TCC_TB:`lf "$TCC_LOCK" sha256`" \
            "$MAKE_TB:`lf "$MAKE_LOCK" sha256`" "$PATCH_TB:`lf "$PATCH_LOCK" sha256`" "$BU_TB:`lf "$BU_LOCK" sha256`"; do
  f=${pair%:*}; want=${pair##*:}
  test -f "$f" || fail "pinned tarball not warm ($f) — run 'td-feed warm sources'"
  test "`sha "$f"`" = "$want" || fail "warmed $f sha256 != lock pin ($want)"
done
test -f "$BOOT_PATCH" || fail "vendored boot patch missing ($BOOT_PATCH)"
test "`sha "$BOOT_PATCH"`" = "$BOOT_PATCH_SHA" || fail "vendored boot patch sha256 != pin ($BOOT_PATCH_SHA)"
echo "   [pinned-input] td-fetched mes/nyacc/tcc/make/patch/binutils tarballs + vendored boot patch match their pins"

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

cpath=`make_curated_path`
for bad in gcc g++ cc guile guix; do test ! -e "$cpath/$bad" || fail "curated PATH still exposes '$bad'"; done
tc=`build_toolchain` || fail "the seed toolchain (brick 0+1) did not build"
mesp=`build_mes_prefix "$tc" "$cpath"` || fail "Mes (MesCC self-host) did not build/install"
TCCD=`mktemp -d`/tcc; build_tcc "$tc" "$cpath" "$mesp" "$TCCD" || fail "MesCC did not build tcc"
MK=`mktemp -d`/makebuild; build_make "$tc" "$cpath" "$mesp" "$TCCD" "$MK" || fail "tcc did not build GNU Make"
PD=`mktemp -d`/patchbuild; build_patch "$cpath" "$mesp" "$TCCD" "$MK" "$PD" || fail "the tcc-built make did not build patch"
BD=`mktemp -d`/binutilsbuild; build_binutils "$cpath" "$mesp" "$TCCD" "$MK" "$PD" "$BD" || fail "the tcc-built make did not build binutils"
trap 'rm -rf "$tc" "$mesp" "`dirname "$TCCD"`" "`dirname "$MK"`" "`dirname "$PD"`" "`dirname "$BD"`" "`dirname "$cpath"`"' EXIT INT TERM

AS="$BD/out/bin/as"; LD="$BD/out/bin/ld"

# --- [no-guix] -------------------------------------------------------------------------------
for art in "$AS" "$LD"; do if grep -q -a '/gnu/store' "$art"; then fail "$art contains /gnu/store bytes"; fi; done
echo "   [no-guix] seed → Mes → MesCC → tcc → make → patch → binutils built with no gcc/guile/guix on PATH; no /gnu/store in as/ld"

# --- [behavioral] as + ld report 2.20.1 and together assemble+link a running i386 program ---------
head -c20 "$AS" | od -An -tx1 | grep -q '7f 45 4c 46 01' || fail "as is not a 32-bit ELF"
av=`env -i "$AS" --version 2>"$BD/as.err" | head -1` || { tail -3 "$BD/as.err" >&2; fail "as did not run"; }
echo "$av" | grep -q '2.20.1' || fail "as --version gave [$av], want 2.20.1"
lv=`env -i "$LD" --version 2>"$BD/ld.err" | head -1` || { tail -3 "$BD/ld.err" >&2; fail "ld did not run"; }
echo "$lv" | grep -q '2.20.1' || fail "ld --version gave [$lv], want 2.20.1"
# durable proof as+ld DO THEIR JOB: assemble a tiny i386 program (raw _start, no libc) and link it.
wd=`mktemp -d`
cat > "$wd/t.s" <<'ASM'
.text
.globl _start
_start:
    movl $1, %eax
    movl $42, %ebx
    int $0x80
ASM
( cd "$wd" && env -i "$AS" --32 -o t.o t.s && env -i "$LD" -m elf_i386 -o t t.o ) >"$wd/al.log" 2>&1 \
  || { tail -8 "$wd/al.log" >&2; rm -rf "$wd"; fail "as/ld could not assemble+link a tiny program"; }
head -c20 "$wd/t" | od -An -tx1 | grep -q '7f 45 4c 46 01' || { rm -rf "$wd"; fail "ld did not emit a 32-bit ELF"; }
set +e; ( cd "$wd" && env -i ./t ); rc=$?; set -e
rm -rf "$wd"
test "$rc" = 42 || fail "the as+ld-built program returned $rc, want 42"
echo "   [behavioral] as→'$av', ld→'$lv'; together they assembled+linked a tiny i386 program that ran and returned 42 — a real assembler+linker built by the seed toolchain"

# --- [repro] a second independent binutils build (same dir) is byte-identical --------------------
as1=`sha "$AS"`; ld1=`sha "$LD"`
build_binutils "$cpath" "$mesp" "$TCCD" "$MK" "$PD" "$BD" || fail "the second binutils build did not run"
test "$as1" = "`sha "$AS"`" || fail "as is NOT reproducible — r1=$as1 r2=`sha "$AS"`"
test "$ld1" = "`sha "$LD"`" || fail "ld is NOT reproducible — r1=$ld1 r2=`sha "$LD"`"
echo "   [repro] two independent binutils builds produce byte-identical as + ld (reproducible)"

echo "PASS: source-bootstrap brick 5 — from the 229-byte seed, the tcc-built GNU Make built GNU"
echo "      Binutils 2.20.1a (guix's binutils-mesboot0): as + ld report 2.20.1 and assemble+link a"
echo "      running i386 program; no gcc/guile/guix, no /gnu/store, reproducible. gcc-mesboot is next."
