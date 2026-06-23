#!/bin/sh
# tests/bootstrap-tcc.sh — source-bootstrap BRICK 4: TinyCC from MesCC. From the 229-byte seed,
# td builds Mes + MesCC (bricks 0-3), installs them, and drives MesCC over the mes-patched TinyCC
# source to produce `tcc` — the first *real* C compiler in the chain — then proves tcc COMPILES +
# LINKS + RUNS a C program. The rung gcc (brick 5) builds onto.
#
# Built i686 (32-bit), as guix's tcc-boot0 does. The mes-patched tcc (0.9.26-1149, the 30-patch
# fork MesCC can compile) + mes-0.27.1 + nyacc-1.00.2 are td-fetched, not vendored
# (seed/sources/*.lock, warmed by tools/warm-bootstrap-sources.sh in check.sh's prelude).
# CRITICAL: mescc runs with MES_ARENA at the guix default (20M cells) — a huge arena overflows the
# 32-bit address space and segfaults; the default fits and compiles tcc.c.
#
# Legs (DURABLE):
#   [pinned-input] the td-fetched mes + nyacc + tcc tarballs match their lock sha256.
#   [no-guix]      built on a curated PATH with gcc/g++/cc/guile/guix DENIED (MesCC+tcc, not a guix
#                  compiler, do the compiling); no /gnu/store byte in tcc.
#   [behavioral]   the seed-built tcc compiles+links a C program and the ELF RUNS returning 42; tcc
#                  is a 32-bit i386 ELF that reports `tcc version 0.9.27`.
#   [repro]        two independent tcc builds (from the same mes prefix) yield a byte-identical tcc.
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
STAGE0=seed/stage0
A=AMD64

# --- [pinned-input] the td-fetched mes + nyacc + tcc tarballs match their locks ---------------
lf() { sed -n "s/^$2 //p" "$1" | head -1; }
MES_LOCK=`ls seed/sources/mes-*.lock | head -1`
NYACC_LOCK=`ls seed/sources/nyacc-*.lock | head -1`
TCC_LOCK=`ls seed/sources/tcc-*.lock | head -1`
test -n "$MES_LOCK" -a -n "$NYACC_LOCK" -a -n "$TCC_LOCK" || fail "missing seed/sources/{mes,nyacc,tcc}-*.lock"
MES_TB=".td-build-cache/sources/`lf "$MES_LOCK" file`"
NYACC_TB=".td-build-cache/sources/`lf "$NYACC_LOCK" file`"
TCC_TB=".td-build-cache/sources/`lf "$TCC_LOCK" file`"
for pair in "$MES_TB:`lf "$MES_LOCK" sha256`" "$NYACC_TB:`lf "$NYACC_LOCK" sha256`" "$TCC_TB:`lf "$TCC_LOCK" sha256`"; do
  f=${pair%:*}; want=${pair##*:}
  test -f "$f" || fail "pinned tarball not warm ($f) — run 'sh tools/warm-bootstrap-sources.sh' (check.sh's prelude does this)"
  test "`sha "$f"`" = "$want" || fail "warmed $f sha256 != lock pin ($want)"
done
echo "   [pinned-input] td-fetched mes + nyacc + tcc tarballs match their lock sha256 — building from the pinned upstream bytes"

# --- a build-driver PATH with NO compiler/interpreter (gcc/cc/guile/guix DENIED) --------------
make_curated_path() {
  cdir=`mktemp -d`/bin; mkdir -p "$cdir"
  oldifs=$IFS; IFS=:
  for d in $PATH; do
    [ -d "$d" ] || continue
    for f in "$d"/*; do
      b=`basename "$f"`
      case "$b" in gcc|g++|cc|c++|cpp|gcc-*|g++-*|clang|clang*|tcc|guile|guild|guile-*|guix|guix-*) continue ;; esac
      [ -e "$cdir/$b" ] || ln -s "$f" "$cdir/$b" 2>/dev/null || true
    done
  done
  IFS=$oldifs; echo "$cdir"
}

# --- seed toolchain (brick 0+1, env -i) -------------------------------------------------------
build_toolchain() {
  tc=`mktemp -d`; cp -a "$STAGE0/." "$tc/"
  chmod +x "$tc/bootstrap-seeds/POSIX/$A/hex0-seed" "$tc/bootstrap-seeds/POSIX/$A/kaem-optional-seed"
  mkdir -p "$tc/$A/artifact" "$tc/$A/bin"
  ( cd "$tc" && env -i ./bootstrap-seeds/POSIX/$A/kaem-optional-seed ./$A/mescc-tools-seed-kaem.kaem \
      && env -i ./$A/artifact/kaem-0 ./$A/mescc-tools-mini-kaem.kaem ) >/dev/null 2>&1 \
    || { echo "seed toolchain build failed in $tc" >&2; return 1; }
  echo "$tc"
}

# canonical-named seed tools (configure.sh resolves M2-Planet/blood-elf/M1/hex2/kaem via command -v)
seedbin_for() {
  tc=$1; sb=`mktemp -d`/seedbin; mkdir -p "$sb"
  ln -sf "$tc/$A/artifact/M2" "$sb/M2-Planet"; ln -sf "$tc/$A/artifact/blood-elf-0" "$sb/blood-elf"
  ln -sf "$tc/$A/bin/M1" "$sb/M1"; ln -sf "$tc/$A/bin/hex2" "$sb/hex2"; ln -sf "$tc/$A/bin/kaem" "$sb/kaem"
  echo "$sb"
}

# --- build + install Mes (i686): MesCC self-hosts mes, emits libc+tcc.a; install to a prefix ---
# Echoes the installed mes prefix (bin/mescc, lib/x86-mes/*, include/, share/mes/module + the
# script's -L dir populated so mescc finds its modules).
build_mes_prefix() {
  tc=$1; cpath=$2; sb=`seedbin_for "$tc"`
  M1B="$tc/$A/bin/M1"; HEX2B="$tc/$A/bin/hex2"; BE="$tc/$A/artifact/blood-elf-0"
  work=`mktemp -d`; tar -xzf "$MES_TB" -C "$work"; m="$work/`tar -tzf "$MES_TB" | head -1 | cut -d/ -f1`"
  tar -xzf "$NYACC_TB" -C "$work"; ny="$work/`tar -tzf "$NYACC_TB" | head -1 | cut -d/ -f1`"
  GLP="$ny/module:$m/mes/module:$m/module"
  ( cd "$m"
    bp="$sb:$cpath"
    PATH="$bp" GUILE_LOAD_PATH="$GLP" MES_PREFIX="$m" MES_ARENA=100000000 MES_MAX_ARENA=100000000 MES_STACK=8000000 \
      GUILE=true CC= MES_FOR_BUILD=mes bash configure.sh --prefix="$m/out" --host=i686-linux-gnu >cfg.log 2>&1 \
      || { echo "mes configure failed" >&2; tail -5 cfg.log >&2; exit 1; }
    for step in bootstrap install; do
      PATH="$bp" GUILE_LOAD_PATH="$GLP" MES_PREFIX="$m" MES_ARENA=100000000 MES_MAX_ARENA=100000000 MES_STACK=8000000 \
        GUILE=true MES_FOR_BUILD=mes M1="$M1B" HEX2="$HEX2B" BLOOD_ELF="$BE" \
        sh "$step.sh" >"$step.log" 2>&1 || { echo "mes $step failed" >&2; tail -8 "$step.log" >&2; exit 1; }
    done ) || return 1
  # populate the mescc script's -L dir (guile_site) with the mes modules + nyacc (mes's load-path
  # for the full module set is flaky; the install leaves the -L dir empty).
  prefix="$m/out"; gsd=`ls -d "$prefix"/share/guile/site/* 2>/dev/null | head -1`
  mkdir -p "$gsd"; cp -a "$prefix/share/mes/module/." "$gsd/" 2>/dev/null; cp -a "$ny/module/." "$gsd/" 2>/dev/null
  test -x "$prefix/bin/mescc" -a -s "$prefix/lib/x86-mes/libc+tcc.a" || { echo "mes install incomplete" >&2; return 1; }
  echo "$prefix"
}

# --- build tcc with MesCC against the installed mes prefix, at a CALLER-GIVEN dir --------------
# MES_ARENA at the guix default (20M cells) — a huge arena overflows 32-bit and segfaults on tcc.c.
# tcc embeds its build prefix (CONFIG_TCC* paths + a crt1.c path), so the two repro builds use the
# SAME dir ($4) — re-extracted each time — to come out byte-identical.
build_tcc() {
  tc=$1; cpath=$2; mesp=$3; t=$4; sb=`seedbin_for "$tc"`
  ln -sf "$mesp/bin/mescc" "$sb/mescc"; ln -sf "$mesp/bin/mes" "$sb/mes"
  NYM=`ls -d "$mesp"/share/guile/site/*/nyacc 2>/dev/null | head -1`; NYM="${NYM%/nyacc}"
  rm -rf "$t"; mkdir -p "$t"; tar -xzf "$TCC_TB" -C "$t" --strip-components=1
  ( cd "$t"
    sed -i 's/volatile//' conftest.c 2>/dev/null || true
    bp="$sb:$cpath"
    env PATH="$bp" MES_PREFIX="$mesp" GUILE_LOAD_PATH="$NYM" \
        host=i686-linux-gnu ONE_SOURCE=true prefix="$t/out" \
      sh configure --cc=mescc --prefix="$t/out" --elfinterp=/lib/mes-loader --crtprefix=. --tccdir=. >cfg.log 2>&1 \
      || { echo "tcc configure failed" >&2; tail -5 cfg.log >&2; exit 1; }
    env PATH="$bp" MES_PREFIX="$mesp" GUILE_LOAD_PATH="$NYM" \
        host=i686-linux-gnu ONE_SOURCE=true prefix="$t/out" \
        MES_ARENA=20000000 MES_MAX_ARENA=20000000 MES_STACK=6000000 \
      sh bootstrap.sh >boot.log 2>&1 || { echo "tcc bootstrap failed" >&2; tail -10 boot.log >&2; exit 1; }
  ) || return 1
  test -x "$t/tcc" || { echo "no tcc binary produced in $t" >&2; return 1; }
}

cpath=`make_curated_path`
for bad in gcc g++ cc guile guix; do test ! -e "$cpath/$bad" || fail "curated PATH still exposes '$bad'"; done
tc=`build_toolchain` || fail "the seed toolchain (brick 0+1) did not build"
mesp=`build_mes_prefix "$tc" "$cpath"` || fail "Mes (MesCC self-host) did not build/install"
# tcc embeds its build prefix, so both repro builds use the SAME dir (re-extracted) to be identical.
TCCB=`mktemp -d`/tccbuild
build_tcc "$tc" "$cpath" "$mesp" "$TCCB" || fail "MesCC did not build tcc"
trap 'rm -rf "$tc" "$mesp" "`dirname "$TCCB"`" "`dirname "$cpath"`"' EXIT INT TERM

# --- [no-guix] -------------------------------------------------------------------------------
TCC="$TCCB/tcc"
if grep -q -a '/gnu/store' "$TCC"; then fail "tcc contains /gnu/store bytes"; fi
echo "   [no-guix] seed → Mes → MesCC → tcc built with no gcc/guile/guix on PATH; no /gnu/store in tcc"

# --- [behavioral] tcc compiles + links + RUNS a C program; it's a 32-bit i386 ELF -------------
head -c20 "$TCC" | od -An -tx1 | grep -q '7f 45 4c 46 01' || fail "tcc is not a 32-bit ELF"
printf 'int main(){return 42;}\n' > "$TCCB/t42.c"
( cd "$TCCB" && env -i ./tcc -static -o t42 t42.c ) >"$TCCB/cc.log" 2>&1 || { tail -5 "$TCCB/cc.log" >&2; fail "seed-built tcc could not compile a C program"; }
test -x "$TCCB/t42" || fail "tcc produced no binary"
set +e; "$TCCB/t42"; rc=$?; set -e
test "$rc" = 42 || fail "the tcc-built program returned $rc, want 42"
"$TCC" -v 2>&1 | grep -q 'tcc version 0.9.27' || fail "tcc -v did not report version 0.9.27"
echo "   [behavioral] the seed-built tcc (32-bit i386 ELF, tcc 0.9.27) compiled a C program that RAN returning 42 — a working real C compiler"

# --- [repro] a second independent tcc build (same build dir) is byte-identical ----------------
sha1=`sha "$TCC"`
build_tcc "$tc" "$cpath" "$mesp" "$TCCB" || fail "the second tcc build did not run"
test "$sha1" = "`sha "$TCC"`" || fail "tcc is NOT reproducible — r1=$sha1 r2=`sha "$TCC"`"
echo "   [repro] two independent tcc builds produce a byte-identical tcc (reproducible)"

echo "PASS: source-bootstrap brick 4 — from the 229-byte seed, td built Mes + MesCC and drove MesCC"
echo "      over the mes-patched TinyCC to produce tcc, the first real C compiler — i686, no"
echo "      gcc/guile/guix on PATH, no /gnu/store bytes, reproducible; tcc compiles a C program"
echo "      that links + RUNS returning 42. The rung gcc (brick 5) builds onto."
