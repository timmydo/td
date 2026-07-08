#!/bin/sh
# tests/harness-loop.sh — the guix-free inner loop body, run INSIDE td's OWN /td/store
# harness by `./check.sh check-harness` (via mk/harness.mk). It proves td's loop SUBSTRATE —
# the busybox + GNU make userland interned at /td/store (gate 420, guix-byte-free) — drives a
# real build with NO guix and NO /gnu/store. This is the container td-builder daily uses
# on a VM with no guix installed.
#
# This script may use ONLY the harness userland (the busybox applets + make + the staged C
# toolchain on the /td/store PATH). No guix, no /gnu/store, no host tools — that is the whole
# point. (td-builder, the engine, joins the IN-harness pillars via rust-store-native rung 3 —
# today it runs host-side as the sandbox provider.)
#
# Legs (DURABLE — no guix oracle in the room):
#   [structural]  inside, /gnu/store + /var/guix are ABSENT and guix is unresolvable.
#   [structural]  the store IS /td/store — the harness busybox lives there.
#   [behavioral]  the /td/store GNU make drives a REAL multi-target build GRAPH over the
#                 busybox userland: prerequisites, pattern rules, a deterministic artifact,
#                 and a correct INCREMENTAL rebuild (a no-op second run; a changed input
#                 rebuilds only its path). This is the loop's core operation, guix-free.
#   [behavioral]  the staged /td/store gcc COMPILES + RUNS real software (Increment 3): it
#                 builds a C program that returns 42 and runs it in the own-root, guix-free —
#                 the guix-free loop builds programs, not just text.
set -eu
fail() { echo "FAIL: $*" >&2; exit 1; }
echo "HARNESS-LOOP-START"

# [structural] no guix substrate inside the container
[ -e /gnu/store ] && fail "/gnu/store is PRESENT inside the harness" || echo "  GNU-ABSENT"
[ -e /var/guix ] && fail "/var/guix is PRESENT inside the harness" || echo "  VARGUIX-ABSENT"
command -v guix >/dev/null 2>&1 && fail "guix is resolvable inside the harness" || echo "  GUIX-ABSENT"

# [structural] the store IS /td/store — the harness userland resolves from there
bb=`command -v busybox` || fail "busybox not resolvable on the harness PATH"
case "$bb" in /td/store/*) echo "  STORE-IS-TDSTORE ($bb)" ;; *) fail "busybox is not from /td/store: $bb" ;; esac
mk=`command -v make` || fail "make not resolvable on the harness PATH"
case "$mk" in /td/store/*) : ;; *) fail "make is not from /td/store: $mk" ;; esac
mv=`make --version 2>/dev/null | { IFS= read -r line || line=; printf '%s\n' "$line"; }`
case "$mv" in "GNU Make 4."*) echo "  MAKE-OK ($mv)" ;; *) fail "harness make did not report GNU Make 4.x: '$mv'" ;; esac

# [behavioral] the /td/store GNU make drives a REAL build graph over the busybox userland.
# A nested make would inherit this outer make's jobserver (a segfault risk / spurious warnings);
# clear the handoff so the sub-build is a clean top-level make.
unset MAKEFLAGS MFLAGS GNUMAKEFLAGS MAKELEVEL || true
# make's default recipe shell is /bin/sh, which does NOT exist in the harness own-root
# (only /td/store), so a fresh sub-make would fail every recipe with 127. Point it at the
# harness busybox sh explicitly (the outer mk/harness.mk gets this from check.sh, but a
# nested `make` does not inherit it).
hsh=`command -v sh` || fail "sh not resolvable on the harness PATH"
work=`mktemp -d 2>/dev/null || echo ./.harness-make.$$`
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/src"
printf 'alpha\n' > "$work/src/a.txt"
printf 'beta\n'  > "$work/src/b.txt"
printf 'gamma\n' > "$work/src/c.txt"
# A genuine dependency graph: each chapter is UPPERCASED (busybox tr) into build/, then the
# chapters are concatenated (busybox cat) into the report — with real prerequisites so make
# rebuilds only what a changed input affects.
cat > "$work/Makefile" <<'MK'
CHAPTERS = a b c
UPS = $(CHAPTERS:%=build/%.up)
all: build/report.txt
build/%.up: src/%.txt
	@mkdir -p build
	tr a-z A-Z < $< > $@
build/report.txt: $(UPS)
	@mkdir -p build
	cat $(UPS) > $@
MK

run_make() { ( cd "$work" && make SHELL="$hsh" "$@" ); }

# 1. First build — make walks the graph and produces the deterministic artifact.
run_make >"$work/log1" 2>&1 || { cat "$work/log1" >&2; fail "harness make: first build failed"; }
got=`cat "$work/build/report.txt"`
want=`printf 'ALPHA\nBETA\nGAMMA\n'`
[ "$got" = "$want" ] || fail "harness make: report wrong after build 1 (got '$got')"
echo "  MAKE-BUILD-OK (graph produced ALPHA/BETA/GAMMA from src via busybox tr+cat)"

# 2. Incremental no-op — nothing changed, make must rebuild nothing.
run_make >"$work/log2" 2>&1 || { cat "$work/log2" >&2; fail "harness make: second build failed"; }
log2=`cat "$work/log2"`
case "$log2" in
  *"Nothing to be done"*|*"is up to date"*) ;;
  *) fail "harness make: a no-op rebuild was not detected (incremental logic broken): $log2" ;;
esac
echo "  MAKE-INCREMENTAL-NOOP-OK (make rebuilt nothing on an unchanged tree)"

# 3. Incremental change — touch ONE input; only its path (a.up) and the report rebuild.
printf 'delta\n' > "$work/src/a.txt"
run_make >"$work/log3" 2>&1 || { cat "$work/log3" >&2; fail "harness make: incremental build failed"; }
got=`cat "$work/build/report.txt"`
want=`printf 'DELTA\nBETA\nGAMMA\n'`
[ "$got" = "$want" ] || fail "harness make: report wrong after changing a.txt (got '$got')"
# The rebuild signature is a chapter's `tr` recipe READING its src/*.txt; the report's `cat`
# line lists build/b.up build/c.up as prerequisites but that is not a rebuild of b/c.
log3=`cat "$work/log3"`
case "$log3" in
  *src/b.txt*|*src/c.txt*) fail "harness make: rebuilt an UNCHANGED chapter (b/c) — incremental tracking is wrong: $log3" ;;
esac
echo "  MAKE-INCREMENTAL-REBUILD-OK (changed a.txt → report=DELTA/BETA/GAMMA; b,c not rebuilt)"

# [behavioral] the harness COMPILES + RUNS real SOFTWARE with the staged /td/store gcc
# (Increment 3) — the guix-free loop builds programs, not just text. Reads the toolchain
# manifest gate 420 (userland-x86_64-store-native) persisted alongside the harness.
mf=.td-build-cache/harness/toolchain
[ -f "$mf" ] || fail "no harness toolchain manifest ($mf) — rebuild via ./check.sh userland-x86_64-store-native"
HT_TARGET=; HT_GCC=; HT_GLIBC=; HT_BU=
. "$mf"
{ [ -n "$HT_TARGET" ] && [ -n "$HT_GCC" ] && [ -n "$HT_GLIBC" ] && [ -n "$HT_BU" ]; } \
  || fail "harness toolchain manifest incomplete ($mf)"
gcc=/td/store/$HT_GCC/bin/$HT_TARGET-gcc
glib=/td/store/$HT_GLIBC
[ -x "$gcc" ] || fail "staged harness gcc not present ($gcc)"
printf 'int main(void){return 42;}\n' > "$work/hello.c"
# Mirror the proven guix-free own-root compile (x86_64_verify_closure): glibc headers/crt/libs
# via -isystem/-B/-L, the dynamic linker + rpath baked to the /td/store x86_64 ld; binutils
# (as/ld) on PATH (the cross gcc also bundles them in its own tooldir).
PATH="/td/store/$HT_BU/bin:$PATH" "$gcc" \
  -isystem "$glib/include" -B"$glib/lib" -L"$glib/lib" -static-libgcc \
  -Wl,--dynamic-linker -Wl,"$glib/lib/ld-linux-x86-64.so.2" \
  -Wl,--enable-new-dtags -Wl,-rpath -Wl,"$glib/lib" \
  -o "$work/hello" "$work/hello.c" 2>"$work/cc.log" \
  || { cat "$work/cc.log" >&2; fail "harness /td/store gcc could not compile hello.c"; }
# set -e safe: the program EXITS 42 by design, so capture it in a `||` list (a bare
# `"$work/hello"; hrc=$?` would abort the script on the expected non-zero exit).
hrc=0; "$work/hello" || hrc=$?
[ "$hrc" = 42 ] || fail "harness-compiled program returned $hrc, want 42"
echo "  COMPILE-RUN-OK (harness /td/store $HT_TARGET-gcc compiled hello.c → ran → 42, no guix, no /gnu/store)"

echo "HARNESS-LOOP-OK"
