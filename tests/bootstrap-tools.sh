#!/bin/sh
# tests/bootstrap-tools.sh — source-bootstrap BRICK 5 (gcc toolchain), tool rungs toward binutils.
# From the 229-byte seed, td builds Mes + MesCC + tcc (bricks 0-4), then the seed-built tcc compiles
# two mesboot tools: GNU gzip 1.2.4 (guix's gzip-mesboot, a scripted tcc build) and pristine TinyCC
# 0.9.27 (guix's tcc-boot — the brick-4 0.9.26 mes-fork tcc compiles pristine 0.9.27, "the fuller
# tcc"). Neither needs make. i686, static. (patch + binutils — both make-driven — are the next PR.)
#
# Legs (DURABLE):
#   [pinned-input] the td-fetched mes + nyacc + tcc + gzip + tcc-0.9.27 tarballs match their locks.
#   [no-guix]      built on a curated PATH with gcc/cc/guile/guix DENIED (tcc compiles the tools);
#                  no /gnu/store byte in gzip / tcc-0.9.27.
#   [behavioral]   the tcc-built gzip reports `gzip 1.2.4`; the fuller tcc-0.9.27 COMPILES + RUNS a
#                  C program returning 33 — a real compiler built by the seed-built compiler.
#   [repro]        gzip and tcc-0.9.27, each rebuilt at the same dir, are byte-identical.
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
STAGE0=seed/stage0
A=AMD64

# --- [pinned-input] the five tarballs match their locks ---------------------------------------
lf() { sed -n "s/^$2 //p" "$1" | head -1; }
MES_LK=`ls seed/sources/mes-*.lock|head -1`; NY_LK=`ls seed/sources/nyacc-*.lock|head -1`
TCC_LK=`ls seed/sources/tcc-0.9.26*.lock|head -1`; GZ_LK=`ls seed/sources/gzip-*.lock|head -1`
T927_LK=`ls seed/sources/tcc-0.9.27*.lock|head -1`
for l in "$MES_LK" "$NY_LK" "$TCC_LK" "$GZ_LK" "$T927_LK"; do test -n "$l" || fail "missing a seed/sources/*.lock"; done
tb() { echo ".td-build-cache/sources/`lf "$1" file`"; }
MES_TB=`tb "$MES_LK"`; NY_TB=`tb "$NY_LK"`; TCC_TB=`tb "$TCC_LK"`; GZ_TB=`tb "$GZ_LK"`; T927_TB=`tb "$T927_LK"`
for pair in "$MES_TB:`lf "$MES_LK" sha256`" "$NY_TB:`lf "$NY_LK" sha256`" "$TCC_TB:`lf "$TCC_LK" sha256`" \
            "$GZ_TB:`lf "$GZ_LK" sha256`" "$T927_TB:`lf "$T927_LK" sha256`"; do
  f=${pair%:*}; want=${pair##*:}
  test -f "$f" || fail "pinned tarball not warm ($f) — run 'sh tools/warm-bootstrap-sources.sh'"
  test "`sha "$f"`" = "$want" || fail "warmed $f sha256 != lock pin ($want)"
done
echo "   [pinned-input] the five td-fetched tarballs (mes/nyacc/tcc/gzip/tcc-0.9.27) match their lock sha256"

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
      && env -i ./$A/artifact/kaem-0 ./$A/mescc-tools-mini-kaem.kaem ) >/dev/null 2>&1 || { echo "seed build failed" >&2; return 1; }
  echo "$tc"
}
seedbin_for() {
  tc=$1; sb=`mktemp -d`/seedbin; mkdir -p "$sb"
  ln -sf "$tc/$A/artifact/M2" "$sb/M2-Planet"; ln -sf "$tc/$A/artifact/blood-elf-0" "$sb/blood-elf"
  ln -sf "$tc/$A/bin/M1" "$sb/M1"; ln -sf "$tc/$A/bin/hex2" "$sb/hex2"; ln -sf "$tc/$A/bin/kaem" "$sb/kaem"; echo "$sb"
}
build_mes_prefix() {
  tc=$1; cpath=$2; sb=`seedbin_for "$tc"`; M1B="$tc/$A/bin/M1"; HEX2B="$tc/$A/bin/hex2"; BE="$tc/$A/artifact/blood-elf-0"
  work=`mktemp -d`; tar -xzf "$MES_TB" -C "$work"; m="$work/`tar -tzf "$MES_TB"|head -1|cut -d/ -f1`"
  tar -xzf "$NY_TB" -C "$work"; ny="$work/`tar -tzf "$NY_TB"|head -1|cut -d/ -f1`"
  GLP="$ny/module:$m/mes/module:$m/module"
  ( cd "$m"; bp="$sb:$cpath"
    PATH="$bp" GUILE_LOAD_PATH="$GLP" MES_PREFIX="$m" MES_ARENA=100000000 MES_MAX_ARENA=100000000 MES_STACK=8000000 \
      GUILE=true CC= MES_FOR_BUILD=mes bash configure.sh --prefix="$m/out" --host=i686-linux-gnu >cfg.log 2>&1 || { echo "mes cfg failed">&2; tail -5 cfg.log>&2; exit 1; }
    for s in bootstrap install; do
      PATH="$bp" GUILE_LOAD_PATH="$GLP" MES_PREFIX="$m" MES_ARENA=100000000 MES_MAX_ARENA=100000000 MES_STACK=8000000 \
        GUILE=true MES_FOR_BUILD=mes M1="$M1B" HEX2="$HEX2B" BLOOD_ELF="$BE" sh "$s.sh" >"$s.log" 2>&1 || { echo "mes $s failed">&2; tail -8 "$s.log">&2; exit 1; }
    done ) || return 1
  prefix="$m/out"; gsd=`ls -d "$prefix"/share/guile/site/* 2>/dev/null|head -1`
  mkdir -p "$gsd"; cp -a "$prefix/share/mes/module/." "$gsd/" 2>/dev/null; cp -a "$ny/module/." "$gsd/" 2>/dev/null
  test -x "$prefix/bin/mescc" || { echo "mes install incomplete">&2; return 1; }; echo "$prefix"
}
build_tcc() {
  tc=$1; cpath=$2; mesp=$3; t=$4; sb=`seedbin_for "$tc"`
  ln -sf "$mesp/bin/mescc" "$sb/mescc"; ln -sf "$mesp/bin/mes" "$sb/mes"
  NYM=`ls -d "$mesp"/share/guile/site/*/nyacc 2>/dev/null|head -1`; NYM="${NYM%/nyacc}"
  rm -rf "$t"; mkdir -p "$t"; tar -xzf "$TCC_TB" -C "$t" --strip-components=1
  ( cd "$t"; sed -i 's/volatile//' conftest.c 2>/dev/null||true; bp="$sb:$cpath"
    env PATH="$bp" MES_PREFIX="$mesp" GUILE_LOAD_PATH="$NYM" host=i686-linux-gnu ONE_SOURCE=true prefix="$t/out" \
      sh configure --cc=mescc --prefix="$t/out" --elfinterp=/lib/mes-loader --crtprefix=. --tccdir=. >cfg.log 2>&1 || { echo "tcc cfg failed">&2; tail -5 cfg.log>&2; exit 1; }
    env PATH="$bp" MES_PREFIX="$mesp" GUILE_LOAD_PATH="$NYM" host=i686-linux-gnu ONE_SOURCE=true prefix="$t/out" \
        MES_ARENA=20000000 MES_MAX_ARENA=20000000 MES_STACK=6000000 sh bootstrap.sh >boot.log 2>&1 || { echo "tcc boot failed">&2; tail -10 boot.log>&2; exit 1; }
  ) || return 1
  test -x "$t/tcc" || { echo "no tcc">&2; return 1; }
}

# --- the two tool builds (each at a CALLER-GIVEN dir; re-extracted for repro) ------------------
# common: crt objects copied in, mes includes, host C_INCLUDE_PATH unset (it leaks unparseable
# glibc headers; guix sets C_INCLUDE_PATH to the mes includes).
build_gzip() {  # scripted tcc build (guix gzip-mesboot)
  cpath=$1; tccd=$2; mesp=$3; g=$4; inc1="$mesp/include"; inc2="$mesp/include/x86"
  rm -rf "$g"; mkdir -p "$g"; tar -xf "$GZ_TB" -C "$g" --strip-components=1
  cp "$tccd"/crt1.o "$tccd"/crti.o "$tccd"/crtn.o "$tccd"/libc.a "$tccd"/libtcc1.a "$g/"
  mkdir -p "$g/bin"; ln -sf "$tccd/tcc" "$g/bin/tcc"
  ( cd "$g"; export PATH="$g/bin:$cpath"; unset C_INCLUDE_PATH CPATH LIBRARY_PATH 2>/dev/null||true
    sed -i 's/^char [*]strlwr/&_tcc_cannot_handle_dupe/' util.c
    files="bits crypt deflate getopt gzip inflate lzw trees unlzh unlzw unpack unzip util zip"
    for x in $files; do tcc -static -L. -I"$inc1" -I"$inc2" -c -D NO_UTIME=1 -D HAVE_UNISTD_H=1 "$x.c" || { echo "gzip compile $x failed">&2; exit 1; }; done
    tcc -static -L. -o gzip `for x in $files; do echo $x.o; done` || { echo "gzip link failed">&2; exit 1; }
  ) || return 1; test -x "$g/gzip" || return 1
}
build_tccboot() {  # pristine tcc 0.9.27, compiled by the brick-4 tcc (guix tcc-boot)
  cpath=$1; tccd=$2; mesp=$3; tb=$4; inc1="$mesp/include"
  rm -rf "$tb"; mkdir -p "$tb"
  # the tcc-0.9.27 tarball is .tar.bz2 but the loop sandbox has no bzip2 on PATH; use the one in the
  # exposed /gnu/store (it's there for the host toolchain) to decompress, then plain tar.
  bz=`command -v bzip2 2>/dev/null || ls /gnu/store/*bzip2*/bin/bzip2 2>/dev/null | head -1`
  test -n "$bz" || { echo "no bzip2 to unpack $T927_TB" >&2; return 1; }
  "$bz" -dc "$T927_TB" | tar -xf - -C "$tb" --strip-components=1 || { echo "tcc-0.9.27 unpack failed" >&2; return 1; }
  pfx="$tb/pfx"; mkdir -p "$pfx/lib" "$pfx/include"
  cp "$tccd"/crt1.o "$tccd"/crti.o "$tccd"/crtn.o "$tccd"/libc.a "$pfx/lib/"; cp -a "$inc1"/. "$pfx/include/" 2>/dev/null
  # the brick-4 tcc (crtprefix=.) links the output in the build dir → crt objects + libs there too
  cp "$tccd"/crt1.o "$tccd"/crti.o "$tccd"/crtn.o "$tccd"/libc.a "$tccd"/libtcc1.a "$tb/"
  mkdir -p "$tb/bin"; ln -sf "$tccd/tcc" "$tb/bin/tcc"
  ( cd "$tb"; export PATH="$tb/bin:$cpath"; unset C_INCLUDE_PATH CPATH LIBRARY_PATH 2>/dev/null||true
    sed -i 's/s->alacarte_link = 1;/&\n    s->static_link = 1;/' libtcc.c
    sh configure --cc=tcc --cpu=i386 --prefix="$tb/out" --elfinterp=/mes/loader --crtprefix="$pfx/lib" --sysincludepaths="$pfx/include" --libpaths="$pfx/lib" >cfg.log 2>&1 || { echo "tcc-boot cfg failed">&2; tail -5 cfg.log>&2; exit 1; }
    tcc -static -D BOOTSTRAP=1 -D ONE_SOURCE=1 -D TCC_TARGET_I386=1 -D CONFIG_TCC_STATIC=1 -D CONFIG_USE_LIBGCC=1 \
      -D "CONFIG_TCCDIR=\"$tb/lib/tcc\"" -D "CONFIG_TCC_CRTPREFIX=\"$pfx/lib:.\"" -D "CONFIG_TCC_ELFINTERP=\"/mes/loader\"" \
      -D "CONFIG_TCC_LIBPATHS=\"$pfx/lib:.\"" -D "CONFIG_TCC_SYSINCLUDEPATHS=\"$pfx/include:.\"" -D "TCC_LIBGCC=\"$pfx/lib/libc.a\"" \
      -I"$inc1" -I"$inc1/x86" -I. -L. -o tcc tcc.c >build.log 2>&1 || { echo "tcc-boot build failed">&2; tail -8 build.log>&2; exit 1; }
    mkdir -p tlib
    ./tcc -static -D TCC_TARGET_I386=1 -c -o libtcc1.o lib/libtcc1.c >>build.log 2>&1 && ./tcc -ar rc libtcc1.a libtcc1.o >>build.log 2>&1 || { echo "tcc-boot libtcc1 failed">&2; exit 1; }
    cp "$pfx"/lib/crt1.o "$pfx"/lib/crti.o "$pfx"/lib/crtn.o "$pfx"/lib/libc.a libtcc1.a tlib/
  ) || return 1; test -x "$tb/tcc" || return 1
}

cpath=`make_curated_path`
for bad in gcc g++ cc guile guix; do test ! -e "$cpath/$bad" || fail "curated PATH still exposes '$bad'"; done
tc=`build_toolchain` || fail "the seed toolchain did not build"
mesp=`build_mes_prefix "$tc" "$cpath"` || fail "Mes did not build/install"
TCCD=`mktemp -d`/tcc; build_tcc "$tc" "$cpath" "$mesp" "$TCCD" || fail "MesCC did not build tcc"
GZ=`mktemp -d`/gz; TB=`mktemp -d`/tb
build_gzip "$cpath" "$TCCD" "$mesp" "$GZ" || fail "tcc did not build gzip"
build_tccboot "$cpath" "$TCCD" "$mesp" "$TB" || fail "tcc did not build pristine tcc 0.9.27"
trap 'rm -rf "$tc" "$mesp" "`dirname "$TCCD"`" "`dirname "$GZ"`" "`dirname "$TB"`" "`dirname "$cpath"`"' EXIT INT TERM

# --- [no-guix] -------------------------------------------------------------------------------
for art in "$GZ/gzip" "$TB/tcc"; do if grep -q -a '/gnu/store' "$art"; then fail "$art contains /gnu/store bytes"; fi; done
echo "   [no-guix] tcc built gzip + tcc-0.9.27 with no gcc/guile/guix on PATH; no /gnu/store bytes"

# --- [behavioral] -----------------------------------------------------------------------------
gv=`env -i "$GZ/gzip" --version 2>&1 | head -1`; echo "$gv" | grep -q 'gzip 1.2.4' || fail "gzip --version gave [$gv], want 'gzip 1.2.4'"   # gzip 1.2.4 prints the banner to stderr
"$TB/tcc" -v 2>&1 | grep -q 'tcc version 0.9.27' || fail "tcc-0.9.27 -v did not report version 0.9.27"
printf 'int main(){return 33;}\n' > "$TB/t33.c"
( cd "$TB" && env -i ./tcc -static -B"$TB/tlib" -I"$TB/pfx/include" -o t33 t33.c ) >"$TB/cc.log" 2>&1 || { tail -5 "$TB/cc.log" >&2; fail "tcc-0.9.27 could not compile a C program"; }
set +e; "$TB/t33"; rc=$?; set -e
test "$rc" = 33 || fail "the tcc-0.9.27-built program returned $rc, want 33"
echo "   [behavioral] gzip→'$gv', and the fuller tcc-0.9.27 compiled+ran a C program returning 33 — a real compiler built by the seed compiler"

# --- [repro] each tool rebuilt at the same dir is byte-identical -------------------------------
g1=`sha "$GZ/gzip"`; build_gzip "$cpath" "$TCCD" "$mesp" "$GZ" || fail "gzip rebuild failed"
test "$g1" = "`sha "$GZ/gzip"`" || fail "gzip is NOT reproducible"
t1=`sha "$TB/tcc"`; build_tccboot "$cpath" "$TCCD" "$mesp" "$TB" || fail "tcc-0.9.27 rebuild failed"
test "$t1" = "`sha "$TB/tcc"`" || fail "tcc-0.9.27 is NOT reproducible"
echo "   [repro] gzip and tcc-0.9.27 each rebuild byte-identical (reproducible)"

echo "PASS: source-bootstrap brick 5 tool rungs — from the 229-byte seed, the seed-built tcc compiled"
echo "      gzip 1.2.4 and the fuller pristine tcc 0.9.27 (which compiles+runs C) — no gcc/guile/guix"
echo "      on PATH, no /gnu/store, reproducible. patch + binutils (make-driven) build on these next."
