#!/bin/sh
# tests/bootstrap-mescc.sh — source-bootstrap BRICK 3: MesCC self-host. From brick 2's seed-built
# mes-m2, td runs Mes's OWN C compiler **MesCC** (Scheme, in the Mes module tree, parsing C with
# nyacc) to compile Mes's whole C library and REBUILD mes as `mes-mescc` — and to produce
# `libc+tcc.a`, the C library TinyCC (brick 4) links against. A C compiler written in Scheme,
# compiling its own runtime, all from the 229-byte seed.
#
# Built i686 (32-bit), as guix's mes-boot does: the Mes/TinyCC layer is 32-bit (gcc later
# cross-builds to 64-bit); the x86_64 MesCC self-host path is immature. The amd64 seed tools
# (M2-Planet/M1/hex2) target i686 via --architecture/defs, so no brick-0/1 rework is needed.
#
# Sources are td-fetched, not vendored (seed/sources/{mes,nyacc}-*.lock, warmed by
# tools/warm-bootstrap-sources.sh in check.sh's prelude). The build driver is an auto-curated
# PATH with gcc/g++/cc/guile/guix DENIED, so MesCC — not a guix compiler/interpreter — does the
# compiling; coreutils/bash/sed/grep remain the §5 toolchain seed (retired last).
#
# Legs (DURABLE):
#   [pinned-input] both warmed tarballs match their lock sha256 (built from the pinned bytes).
#   [no-guix]      no gcc/guile/guix on the build PATH; no /gnu/store byte in mes-mescc.
#   [behavioral]   the MesCC-built mes-mescc RUNS as a Scheme interpreter (display + arithmetic),
#                  and libc+tcc.a is a real, non-empty ar archive — MesCC works + emits the tcc lib.
#   [repro]        two independent MesCC self-host builds yield a byte-identical mes-mescc.
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
STAGE0=seed/stage0
A=AMD64

# --- [pinned-input] the td-fetched mes + nyacc tarballs match their locks ---------------------
lock_field() { sed -n "s/^$2 //p" "$1" | head -1; }
MES_LOCK=`ls seed/sources/mes-*.lock 2>/dev/null | head -1`
NYACC_LOCK=`ls seed/sources/nyacc-*.lock 2>/dev/null | head -1`
test -n "$MES_LOCK" -a -n "$NYACC_LOCK" || fail "missing seed/sources/{mes,nyacc}-*.lock"
MES_TB=".td-build-cache/sources/`lock_field "$MES_LOCK" file`"
NYACC_TB=".td-build-cache/sources/`lock_field "$NYACC_LOCK" file`"
for pair in "$MES_TB:`lock_field "$MES_LOCK" sha256`" "$NYACC_TB:`lock_field "$NYACC_LOCK" sha256`"; do
  f=${pair%:*}; want=${pair##*:}
  test -f "$f" || fail "pinned tarball not warm ($f) — run 'sh tools/warm-bootstrap-sources.sh' (check.sh's prelude does this)"
  test "`sha "$f"`" = "$want" || fail "warmed $f sha256 != lock pin ($want)"
done
echo "   [pinned-input] td-fetched mes + nyacc tarballs match their lock sha256 — building from the pinned upstream bytes"

# --- a build-driver PATH with NO compiler/interpreter (gcc/g++/cc/cpp/guile/guix DENIED) ------
# Everything else on the sandbox PATH (bash/coreutils/sed/grep — the §5 seed) is allowed; the
# ONLY C compiler reachable is MesCC, and no guile/guix process can run. Echoes the curated dir.
make_curated_path() {
  cdir=`mktemp -d`/bin; mkdir -p "$cdir"
  oldifs=$IFS; IFS=:
  for d in $PATH; do
    [ -d "$d" ] || continue
    for f in "$d"/*; do
      b=`basename "$f"`
      case "$b" in
        gcc|g++|cc|c++|cpp|gcc-*|g++-*|clang|clang*|tcc|guile|guild|guile-*|guix|guix-*) continue ;;
      esac
      [ -e "$cdir/$b" ] || ln -s "$f" "$cdir/$b" 2>/dev/null || true
    done
  done
  IFS=$oldifs
  echo "$cdir"
}

# --- build the seed toolchain (brick 0+1, env -i: no guix process) ----------------------------
build_toolchain() {
  tc=`mktemp -d`
  cp -a "$STAGE0/." "$tc/"
  chmod +x "$tc/bootstrap-seeds/POSIX/$A/hex0-seed" "$tc/bootstrap-seeds/POSIX/$A/kaem-optional-seed"
  mkdir -p "$tc/$A/artifact" "$tc/$A/bin"
  ( cd "$tc" \
      && env -i ./bootstrap-seeds/POSIX/$A/kaem-optional-seed ./$A/mescc-tools-seed-kaem.kaem \
      && env -i ./$A/artifact/kaem-0 ./$A/mescc-tools-mini-kaem.kaem ) >/dev/null 2>&1 \
    || { echo "seed toolchain build failed in $tc" >&2; return 1; }
  echo "$tc"
}

# --- MesCC self-host: configure (--host=i686) + bootstrap.sh -> mes-mescc + libc+tcc.a ---------
# Echoes the mes build dir (holds bin/mes-mescc, mescc-lib/x86-mes/libc+tcc.a, and is MES_PREFIX).
build_mescc() {
  tc=$1; cpath=$2
  M2P="$tc/$A/artifact/M2"; BE="$tc/$A/artifact/blood-elf-0"; M1B="$tc/$A/bin/M1"; HEX2B="$tc/$A/bin/hex2"; KAEMB="$tc/$A/bin/kaem"
  # configure.sh resolves the stage0 tools by their CANONICAL names (M2-Planet/blood-elf/M1/hex2/
  # kaem) via `command -v`; the seed builds them as M2/blood-elf-0/… — so expose canonical symlinks.
  sb=`mktemp -d`/seedbin; mkdir -p "$sb"
  ln -sf "$M2P" "$sb/M2-Planet"; ln -sf "$BE" "$sb/blood-elf"
  ln -sf "$M1B" "$sb/M1"; ln -sf "$HEX2B" "$sb/hex2"; ln -sf "$KAEMB" "$sb/kaem"
  bpath="$sb:$cpath"
  work=`mktemp -d`
  tar -xzf "$MES_TB" -C "$work"; m="$work/`tar -tzf "$MES_TB" | head -1 | cut -d/ -f1`"
  tar -xzf "$NYACC_TB" -C "$work"; ny="$work/`tar -tzf "$NYACC_TB" | head -1 | cut -d/ -f1`"
  test -f "$m/configure.sh" -a -d "$ny/module/nyacc" || { echo "unpack failed ($m / $ny)" >&2; return 1; }
  # env: curated PATH (no gcc/guile/guix) + seed tools; force the MesCC path; absolute seed tools
  # (mes's system* does not search PATH); nyacc + mes modules for MesCC's C parser.
  GLP="$ny/module:$m/mes/module:$m/module"
  ( cd "$m"
    PATH="$bpath" \
    GUILE_LOAD_PATH="$GLP" MES_PREFIX="$m" MES_ARENA=100000000 MES_MAX_ARENA=100000000 MES_STACK=8000000 \
    GUILE=true CC= MES_FOR_BUILD=mes \
      bash configure.sh --prefix="$m/out" --host=i686-linux-gnu >configure.log 2>&1 \
      || { echo "mes configure failed" >&2; tail -5 configure.log >&2; exit 1; }
    PATH="$bpath" \
    GUILE_LOAD_PATH="$GLP" MES_PREFIX="$m" MES_ARENA=100000000 MES_MAX_ARENA=100000000 MES_STACK=8000000 \
    GUILE=true MES_FOR_BUILD=mes M1="$M1B" HEX2="$HEX2B" BLOOD_ELF="$BE" KAEM="$KAEMB" M2_PLANET="$M2P" \
      sh bootstrap.sh >bootstrap.log 2>&1 \
      || { echo "mes MesCC bootstrap failed" >&2; tail -10 bootstrap.log >&2; exit 1; }
  ) || return 1
  echo "$m"
}

cpath=`make_curated_path`
# guard: the curated PATH really excludes the compilers/interpreter
for bad in gcc g++ cc guile guix; do
  test ! -e "$cpath/$bad" || fail "curated build PATH still exposes '$bad' — would not be a guix-free compile"
done
tc=`build_toolchain` || fail "the seed toolchain (brick 0+1) did not build (env -i, guix off)"
m1=`build_mescc "$tc" "$cpath"` || fail "MesCC self-host did not build mes-mescc"
trap 'rm -rf "$tc" "$m1" "${m2:-}" "`dirname "$cpath"`"' EXIT INT TERM

# --- [no-guix] -------------------------------------------------------------------------------
MESCC="$m1/bin/mes-mescc"; LIBTCC="$m1/mescc-lib/x86-mes/libc+tcc.a"
test -x "$MESCC" || fail "no mes-mescc produced"
if grep -q -a '/gnu/store' "$MESCC"; then fail "mes-mescc contains /gnu/store bytes"; fi
echo "   [no-guix] MesCC self-host ran with no gcc/guile/guix on PATH; mes-mescc has no /gnu/store bytes"

# --- [behavioral] mes-mescc is a real interpreter + libc+tcc.a is a real archive --------------
LP="GUILE_LOAD_PATH=$m1/mes/module:$m1/module"; MP="MES_PREFIX=$m1"
out=`env -i "$MP" "$LP" "$MESCC" -c "(display 'MesCC-self-host!) (newline)" 2>"$m1/run.err"` \
  || { tail -5 "$m1/run.err" >&2; fail "mes-mescc (MesCC-built) failed to evaluate a display expression"; }
test "$out" = "MesCC-self-host!" || fail "mes-mescc display gave [$out], want [MesCC-self-host!]"
arith=`env -i "$MP" "$LP" "$MESCC" -c "(display (* 6 7)) (newline)" 2>>"$m1/run.err"` || { tail -5 "$m1/run.err" >&2; fail "mes-mescc arithmetic failed"; }
test "$arith" = "42" || fail "mes-mescc arithmetic gave [$arith], want [42]"
test -s "$LIBTCC" || fail "no libc+tcc.a produced (MesCC did not build the tcc library)"
# mesar emits a stage0 M1/hex2-format object archive (`:label` defs + hex), not GNU `!<arch>`.
# Assert MesCC actually compiled the library: the core libc functions are defined, plus the
# tcc-specific `abtod` that distinguishes libc+tcc.a from plain libc.a.
for sym in strlen malloc memcpy abtod; do
  grep -q ":$sym\b" "$LIBTCC" || fail "libc+tcc.a is missing the compiled symbol :$sym — MesCC did not build the tcc library"
done
echo "   [behavioral] the MesCC-built mes-mescc evaluates Scheme (MesCC-self-host!, (* 6 7)→42) and libc+tcc.a defines the compiled libc (strlen/malloc/memcpy) + tcc's abtod — MesCC works"

# --- [repro] a second independent MesCC self-host build is byte-identical ----------------------
m2=`build_mescc "$tc" "$cpath"` || fail "the second MesCC self-host build did not run"
test "`sha "$MESCC"`" = "`sha "$m2/bin/mes-mescc"`" \
  || fail "mes-mescc is NOT reproducible — r1=`sha "$MESCC"` r2=`sha "$m2/bin/mes-mescc"`"
echo "   [repro] two independent MesCC self-host builds produce a byte-identical mes-mescc (reproducible)"

echo "PASS: source-bootstrap brick 3 — from the 229-byte seed, td ran Mes's OWN C compiler (MesCC,"
echo "      written in Scheme) to compile Mes's libc and rebuild mes as mes-mescc, and to emit"
echo "      libc+tcc.a — i686, no gcc/guile/guix on PATH, no /gnu/store bytes, reproducible. The"
echo "      Scheme C compiler self-hosts; TinyCC (brick 4) links libc+tcc.a next."
