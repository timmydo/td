#!/bin/sh
# tests/bootstrap-cc.sh — source-bootstrap BRICK 1: from brick 0's seed-built kaem-0, drive the
# stage0-posix chain (hex1→hex2→M0→cc_amd64→M2-Planet) to a MINIMAL C COMPILER + the core
# mescc-tools (M1 assembler, hex2 linker, kaem) — all from the 229-byte seed, guix-free. The
# milestone: a working C toolchain (compile → assemble → link → RUN a C program) bootstrapped
# from the auditable seed, no /gnu/store, no guix process.
#
# Legs (all DURABLE — the seed chain is the bottom; no guix oracle):
#   [no-guix]    the whole chain runs with guix/Guile scrubbed from env, from brick-0's seeds +
#                vendored hex/C source — no /gnu/store in the produced M2-Planet.
#   [behavioral] the seed-built M2-Planet COMPILES a C program, M1+hex2 assemble+link it to an
#                ELF, and the ELF RUNS and returns the expected value — a real working compiler.
#   [repro]      two independent runs produce a byte-identical M2-Planet.
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }   # cmp/diffutils is absent from the loop sandbox
SEED=seed/stage0
A=AMD64; M=M2libc/amd64

# Build the stage0 chain up to M2-Planet + mescc-tools in a fresh scratch with guix/Guile
# SCRUBBED from env (env -i: the static seeds/tools exec their inputs by relative path, so a
# green build proves NO guix process). Echoes the run dir.
build_cc() {
  o=`mktemp -d`
  cp -a "$SEED/." "$o/"
  chmod +x "$o/bootstrap-seeds/POSIX/AMD64/hex0-seed" "$o/bootstrap-seeds/POSIX/AMD64/kaem-optional-seed"
  mkdir -p "$o/$A/artifact" "$o/$A/bin"
  ( cd "$o" \
      && env -i ./bootstrap-seeds/POSIX/AMD64/kaem-optional-seed ./$A/mescc-tools-seed-kaem.kaem \
      && env -i ./$A/artifact/kaem-0 ./$A/mescc-tools-mini-kaem.kaem ) >/dev/null 2>&1 \
    || { echo "stage0 chain build failed in $o" >&2; return 1; }
  echo "$o"
}

r1=`build_cc` || fail "the stage0 → M2-Planet chain did not build (guix/Guile off env)"
trap 'rm -rf "$r1" "${r2:-}"' EXIT INT TERM
test -x "$r1/$A/artifact/M2" || fail "no M2-Planet (M2) produced"
for t in M1 hex2 kaem; do test -x "$r1/$A/bin/$t" || fail "mescc-tools $t not produced"; done
if grep -q -a '/gnu/store' "$r1/$A/artifact/M2"; then fail "M2-Planet contains /gnu/store bytes"; fi
echo "   [no-guix] seed→kaem-0→M2-Planet + mescc-tools (M1/hex2/kaem) built with guix/Guile off env — no /gnu/store in M2-Planet"

# --- [behavioral] compile + assemble + link + RUN a C program with the seed-built toolchain ---
printf 'int main() { return 7; }\n' > "$r1/$A/artifact/prog.c"
# compile (M2-Planet) -> footer (blood-elf) -> assemble (M1): these must succeed.
( cd "$r1" \
  && env -i ./$A/artifact/M2 --architecture amd64 -f ./$M/linux/bootstrap.c -f ./$A/artifact/prog.c --bootstrap-mode -o ./$A/artifact/prog.M1 \
  && env -i ./$A/artifact/blood-elf-0 --64 --little-endian -f ./$A/artifact/prog.M1 -o ./$A/artifact/prog-footer.M1 \
  && env -i ./$A/bin/M1 --architecture amd64 --little-endian -f ./$M/amd64_defs.M1 -f ./$M/libc-core.M1 -f ./$A/artifact/prog.M1 -f ./$A/artifact/prog-footer.M1 -o ./$A/artifact/prog.hex2 ) >"$r1/cc.log" 2>&1 \
  || { echo "--- compile/assemble stderr ---" >&2; tail -8 "$r1/cc.log" >&2; fail "the seed-built M2-Planet/M1 could not compile+assemble the C program"; }
# link (hex2): it prints a benign "ELF_data is not valid" warning + exits 1 for this minimal
# program but still writes a WORKING ELF — so the real proof is not hex2's exit code, it is that
# the produced binary RUNS and returns the right value.
( cd "$r1" && env -i ./$A/bin/hex2 --architecture amd64 --little-endian --base-address 0x00600000 -f ./$M/ELF-amd64.hex2 -f ./$A/artifact/prog.hex2 -o ./$A/artifact/prog ) >>"$r1/cc.log" 2>&1 || true
test -s "$r1/$A/artifact/prog" || { tail -8 "$r1/cc.log" >&2; fail "hex2 produced no linked binary"; }
chmod +x "$r1/$A/artifact/prog"
set +e; "$r1/$A/artifact/prog"; rc=$?; set -e
test "$rc" = 7 || fail "the seed-built C program returned $rc, want 7 (the compiler/linker mis-built it)"
echo "   [behavioral] M2-Planet compiled a C program, M1+hex2 assembled+linked it to an ELF, and it RAN returning 7 — a working seed-built C toolchain"

# --- [repro] a second independent chain build yields a byte-identical M2-Planet ---------------
r2=`build_cc` || fail "the second chain build did not run"
test "`sha "$r1/$A/artifact/M2"`" = "`sha "$r2/$A/artifact/M2"`" \
  && test "`sha "$r1/$A/bin/M1"`" = "`sha "$r2/$A/bin/M1"`" \
  || fail "the stage0 chain is NOT reproducible — r1 M2=`sha "$r1/$A/artifact/M2"` | r2 M2=`sha "$r2/$A/artifact/M2"`"
echo "   [repro] two independent stage0 chain builds produce a byte-identical M2-Planet + M1 (reproducible)"

echo "PASS: source-bootstrap brick 1 — from the 229-byte seed (brick 0), td drove the stage0-posix"
echo "      chain to a MINIMAL C COMPILER (M2-Planet) + the core mescc-tools (M1/hex2/kaem), guix"
echo "      off env; the compiler compiles a C program that links + RUNS correctly, and the build"
echo "      is reproducible. A self-built, guix-free C toolchain — the rung gcc/glibc climb onto."
