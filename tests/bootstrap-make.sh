#!/bin/sh
# tests/bootstrap-make.sh — source-bootstrap BRICK 5 (gcc toolchain), first rung: the seed-built
# TinyCC (brick 4) compiles GNU Make. From the 229-byte seed, td builds Mes + MesCC + tcc (bricks
# 0-4), then drives tcc (CC=tcc) over the GNU Make 3.80 source to produce a working `make` — tcc's
# first substantial real-program build, and the build tool the gcc/binutils rungs need. Exactly
# guix's make-mesboot0. The rung binutils + gcc build onto this.
#
# i686, static (brick-4 tcc links the mes libc statically — no /lib/mes-loader needed on the host).
# Sources (mes + nyacc + tcc + make) are td-fetched, not vendored. MES_ARENA stays at the guix
# default for the tcc layer (a big arena overflows 32-bit — see brick 4).
#
# Legs (DURABLE):
#   [pinned-input] the td-fetched mes + nyacc + tcc + make tarballs match their lock sha256.
#   [no-guix]      built on a curated PATH with gcc/g++/cc/guile/guix DENIED (tcc, not a guix
#                  compiler, compiles make); no /gnu/store byte in make.
#   [behavioral]   the tcc-built make is a 32-bit i386 ELF that RUNS and reports `GNU Make 3.80`.
#   [repro]        two independent make builds (same dir) yield a byte-identical make.
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
STAGE0=seed/stage0
A=AMD64

# --- [pinned-input] mes + nyacc + tcc + make tarballs match their locks ------------------------
lf() { sed -n "s/^$2 //p" "$1" | head -1; }
MES_LOCK=`ls seed/sources/mes-*.lock | head -1`; NYACC_LOCK=`ls seed/sources/nyacc-*.lock | head -1`
TCC_LOCK=`ls seed/sources/tcc-*.lock | head -1`; MAKE_LOCK=`ls seed/sources/make-*.lock | head -1`
for l in "$MES_LOCK" "$NYACC_LOCK" "$TCC_LOCK" "$MAKE_LOCK"; do test -n "$l" || fail "missing a seed/sources/*.lock"; done
MES_TB=".td-build-cache/sources/`lf "$MES_LOCK" file`"; NYACC_TB=".td-build-cache/sources/`lf "$NYACC_LOCK" file`"
TCC_TB=".td-build-cache/sources/`lf "$TCC_LOCK" file`"; MAKE_TB=".td-build-cache/sources/`lf "$MAKE_LOCK" file`"
for pair in "$MES_TB:`lf "$MES_LOCK" sha256`" "$NYACC_TB:`lf "$NYACC_LOCK" sha256`" "$TCC_TB:`lf "$TCC_LOCK" sha256`" "$MAKE_TB:`lf "$MAKE_LOCK" sha256`"; do
  f=${pair%:*}; want=${pair##*:}
  test -f "$f" || fail "pinned tarball not warm ($f) — run 'td-feed warm sources'"
  test "`sha "$f"`" = "$want" || fail "warmed $f sha256 != lock pin ($want)"
done
echo "   [pinned-input] td-fetched mes + nyacc + tcc + make tarballs match their lock sha256"

# --- curated build-driver PATH (gcc/cc/guile/guix DENIED) -------------------------------------
make_curated_path() {
  cdir=`mktemp -d`/bin; mkdir -p "$cdir"; oldifs=$IFS; IFS=:
  for d in $PATH; do [ -d "$d" ] || continue; for f in "$d"/*; do b=`basename "$f"`
    case "$b" in gcc|g++|cc|c++|cpp|gcc-*|g++-*|clang|clang*|tcc|guile|guild|guile-*|guix|guix-*) continue ;; esac
    [ -e "$cdir/$b" ] || ln -s "$f" "$cdir/$b" 2>/dev/null || true; done; done
  IFS=$oldifs; echo "$cdir"
}
# --- seed toolchain (brick 0+1) + canonical seedbin -------------------------------------------
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
# --- build + install Mes (i686); returns the prefix (mescc + libc+tcc.a + modules) -------------
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
# --- build tcc (brick 4) at a given dir; leaves crt1.o/crti.o/crtn.o/libc.a + tcc there ---------
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
# --- build GNU Make with tcc, at a CALLER-GIVEN dir (re-extracted for repro) -------------------
# brick-4 tcc has crtprefix='.', so crt1.o/crti.o/crtn.o/libc.a are copied into the make dir; -static
# avoids the /lib/mes-loader interpreter; the mes include dirs feed CPP. (guix's make-mesboot0.)
build_make() {
  tc=$1; cpath=$2; mesp=$3; tccd=$4; mk=$5
  rm -rf "$mk"; mkdir -p "$mk"; tar -xzf "$MAKE_TB" -C "$mk" --strip-components=1
  cp "$tccd"/crt1.o "$tccd"/crti.o "$tccd"/crtn.o "$tccd"/libc.a "$tccd"/libtcc1.a "$mk/"
  mkdir -p "$mk/bin"; ln -sf "$tccd/tcc" "$mk/bin/tcc"
  inc1="$mesp/include"; inc2="$mesp/include/x86"
  ( cd "$mk"; bp="$mk/bin:$cpath"
    # configure runs sub-scripts (config.sub/config.guess) via ${CONFIG_SHELL-/bin/sh}; the loop
    # sandbox has no /bin/sh, so point CONFIG_SHELL at the curated sh.
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

cpath=`make_curated_path`
for bad in gcc g++ cc guile guix; do test ! -e "$cpath/$bad" || fail "curated PATH still exposes '$bad'"; done
tc=`build_toolchain` || fail "the seed toolchain (brick 0+1) did not build"
mesp=`build_mes_prefix "$tc" "$cpath"` || fail "Mes (MesCC self-host) did not build/install"
TCCD=`mktemp -d`/tcc; build_tcc "$tc" "$cpath" "$mesp" "$TCCD" || fail "MesCC did not build tcc"
MK=`mktemp -d`/makebuild; build_make "$tc" "$cpath" "$mesp" "$TCCD" "$MK" || fail "tcc did not build GNU Make"
trap 'rm -rf "$tc" "$mesp" "`dirname "$TCCD"`" "`dirname "$MK"`" "`dirname "$cpath"`"' EXIT INT TERM

# --- [no-guix] -------------------------------------------------------------------------------
MAKE="$MK/make"
if grep -q -a '/gnu/store' "$MAKE"; then fail "make contains /gnu/store bytes"; fi
echo "   [no-guix] seed → Mes → MesCC → tcc → make built with no gcc/guile/guix on PATH; no /gnu/store in make"

# --- [behavioral] the tcc-built make is a 32-bit ELF that runs + reports GNU Make 3.80 ---------
head -c20 "$MAKE" | od -An -tx1 | grep -q '7f 45 4c 46 01' || fail "make is not a 32-bit ELF"
ver=`env -i "$MAKE" --version 2>"$MK/run.err" | head -1` || { tail -3 "$MK/run.err" >&2; fail "the tcc-built make did not run"; }
echo "$ver" | grep -q 'GNU Make 3.80' || fail "make --version gave [$ver], want 'GNU Make 3.80'"
echo "   [behavioral] the tcc-built make (32-bit i386 ELF) RUNS and reports '$ver' — tcc compiled a real program"

# --- [repro] a second independent make build (same dir) is byte-identical ----------------------
sha1=`sha "$MAKE"`
build_make "$tc" "$cpath" "$mesp" "$TCCD" "$MK" || fail "the second make build did not run"
test "$sha1" = "`sha "$MAKE"`" || fail "make is NOT reproducible — r1=$sha1 r2=`sha "$MAKE"`"
echo "   [repro] two independent make builds produce a byte-identical make (reproducible)"

echo "PASS: source-bootstrap brick 5 (first rung) — from the 229-byte seed, td built Mes + MesCC +"
echo "      tcc, then tcc compiled GNU Make 3.80 — a working 32-bit make, no gcc/guile/guix on PATH,"
echo "      no /gnu/store bytes, reproducible. tcc's first real-program build; binutils + gcc next."
