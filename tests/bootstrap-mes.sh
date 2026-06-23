#!/bin/sh
# tests/bootstrap-mes.sh — source-bootstrap BRICK 2: from the 229-byte seed, td builds a working
# GNU Mes Scheme interpreter. Brick 1 turns the seed into M2-Planet (a minimal C compiler) +
# mescc-tools (M1 assembler, hex2 linker, blood-elf); this gate drives those over the vendored
# GNU Mes source (seed/mes, pinned mes f244b141 / 0.27.1) to compile + link `mes-m2`, then proves
# it actually evaluates Scheme — all guix-free, reproducible. The rung tinycc (brick 3) climbs onto.
#
# Legs (all DURABLE — the seed chain is the bottom; no guix oracle):
#   [no-guix]    the whole chain (seed → M2-Planet/mescc-tools → mes-m2) runs with guix/Guile
#                scrubbed from env; no /gnu/store byte in the produced mes-m2.
#   [behavioral] the seed-built mes-m2 RUNS as a Scheme interpreter — it evaluates (display ...)
#                and arithmetic from its own vendored module tree and prints the right answers.
#   [repro]      two independent mes builds from the same seed toolchain yield a byte-identical mes-m2.
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }   # cmp/diffutils is absent from the loop sandbox
STAGE0=seed/stage0
MES=seed/mes
A=AMD64
MES_VERSION=0.27.1

# --- build the seed toolchain (brick 0 + brick 1): seed → kaem-0 → M2-Planet + mescc-tools ----
# Echoes an absolute toolchain dir holding artifact/M2 (M2-Planet), artifact/blood-elf-0, bin/M1,
# bin/hex2. env -i on every seed/tool exec proves NO guix process is in the chain.
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

# --- build mes-m2 from the vendored mes source with a given seed toolchain --------------------
# Echoes an absolute mes scratch dir holding bin/mes-m2 (and serving as its MES_PREFIX).
build_mes() {
  tc=$1
  M2P="$tc/$A/artifact/M2"; BE="$tc/$A/artifact/blood-elf-0"
  M1="$tc/$A/bin/M1"; HEX2="$tc/$A/bin/hex2"
  m=`mktemp -d`
  cp -a "$MES/." "$m/"
  mkdir -p "$m/include/mes" "$m/include/arch" "$m/m2" "$m/bin"
  # config.h + arch headers — generated exactly as upstream configure.sh does (non-system libc).
  printf '#undef SYSTEM_LIBC\n#define MES_VERSION "%s"\n' "$MES_VERSION" > "$m/include/mes/config.h"
  cp -f "$m/include/linux/x86_64/kernel-stat.h" "$m/include/linux/x86_64/signal.h" \
        "$m/include/linux/x86_64/syscall.h" "$m/include/arch/"
  ( cd "$m"
    # compile every vendored translation unit (manifest order) -> mes.M1
    set -- --debug --architecture amd64 -D __x86_64__=1 -D __linux__=1
    while IFS= read -r f; do [ -n "$f" ] && set -- "$@" -f "$f"; done < m2planet.files
    env -i "$M2P" "$@" -o m2/mes.M1 \
      && env -i "$BE" --64 --little-endian -f m2/mes.M1 -o m2/mes.blood-elf-M1 \
      && env -i "$M1" --architecture amd64 --little-endian \
           -f lib/m2/x86_64/x86_64_defs.M1 -f lib/x86_64-mes/x86_64.M1 \
           -f lib/linux/x86_64-mes-m2/crt1.M1 -f m2/mes.M1 -f m2/mes.blood-elf-M1 -o m2/mes.hex2 \
      && env -i "$HEX2" --architecture amd64 --little-endian --base-address 0x1000000 \
           -f lib/m2/x86_64/ELF-x86_64.hex2 -f m2/mes.hex2 -o bin/mes-m2 ) >"$m/build.log" 2>&1 \
    || { echo "mes build failed in $m" >&2; tail -8 "$m/build.log" >&2; return 1; }
  chmod +x "$m/bin/mes-m2"
  echo "$m"
}

tc=`build_toolchain` || fail "the seed toolchain (brick 0+1) did not build (guix/Guile off env)"
m1dir=`build_mes "$tc"` || fail "mes-m2 did not build from the seed toolchain"
trap 'rm -rf "$tc" "$m1dir" "${m2dir:-}"' EXIT INT TERM

# --- [no-guix] -------------------------------------------------------------------------------
test -x "$m1dir/bin/mes-m2" || fail "no mes-m2 produced"
if grep -q -a '/gnu/store' "$m1dir/bin/mes-m2"; then fail "mes-m2 contains /gnu/store bytes"; fi
echo "   [no-guix] seed → M2-Planet/mescc-tools → mes-m2 built with guix/Guile off env — no /gnu/store in mes-m2"

# --- [behavioral] mes-m2 is a real Scheme interpreter ----------------------------------------
# mes-m2 finds its boot via MES_PREFIX and resolves (use-modules ...) via GUILE_LOAD_PATH; both
# must be absolute since the gate runs it from the repo root, not the mes scratch (as kaem.run does).
MP="MES_PREFIX=$m1dir"; LP="GUILE_LOAD_PATH=$m1dir/mes/module:$m1dir/module"
out=`env -i "$MP" "$LP" "$m1dir/bin/mes-m2" -c "(display 'Hello,M2-mes!) (newline)" 2>"$m1dir/run.err"` \
  || { tail -5 "$m1dir/run.err" >&2; fail "seed-built mes-m2 failed to evaluate a display expression"; }
test "$out" = "Hello,M2-mes!" || fail "mes-m2 display gave [$out], want [Hello,M2-mes!]"
arith=`env -i "$MP" "$LP" "$m1dir/bin/mes-m2" -c "(display (+ 1 2 3 4)) (newline)" 2>>"$m1dir/run.err"` \
  || { tail -5 "$m1dir/run.err" >&2; fail "seed-built mes-m2 failed to evaluate arithmetic"; }
test "$arith" = "10" || fail "mes-m2 arithmetic gave [$arith], want [10]"
echo "   [behavioral] the seed-built mes-m2 evaluates Scheme from its own module tree: (display 'Hello,M2-mes!)→Hello,M2-mes! and (+ 1 2 3 4)→10 — a working interpreter"

# --- [repro] a second independent mes build (same seed toolchain) is byte-identical -----------
m2dir=`build_mes "$tc"` || fail "the second mes build did not run"
test "`sha "$m1dir/bin/mes-m2"`" = "`sha "$m2dir/bin/mes-m2"`" \
  || fail "mes-m2 is NOT reproducible — r1=`sha "$m1dir/bin/mes-m2"` r2=`sha "$m2dir/bin/mes-m2"`"
echo "   [repro] two independent mes builds produce a byte-identical mes-m2 (reproducible)"

echo "PASS: source-bootstrap brick 2 — from the 229-byte seed, td drove M2-Planet + mescc-tools over"
echo "      the vendored GNU Mes source to a working Scheme interpreter (mes-m2); it evaluates Scheme,"
echo "      carries no /gnu/store bytes, and is reproducible. The rung tinycc (brick 3) builds onto."
