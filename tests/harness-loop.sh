#!/bin/sh
# tests/harness-loop.sh — the guix-free inner loop body, run INSIDE td's OWN /td/store
# harness by `./check.sh check-harness` (via mk/harness.mk). It proves td's loop SUBSTRATE —
# the busybox + GNU make userland interned at /td/store (gate 420, guix-byte-free) — drives a
# real build with NO guix and NO /gnu/store. This is the container ci/daily-full-suite.sh uses
# on a VM with no guix installed.
#
# This script may use ONLY the harness userland (the busybox applets + make on the /td/store
# PATH). No guix, no /gnu/store, no host tools — that is the whole point. (td-builder, the
# engine, joins the IN-harness pillars via rust-store-native rung 3 — today it runs host-side
# as the sandbox provider. A COMPILER is not in the harness yet either: expanding the /td/store
# userland to the loop toolchain — gcc, so the guix-free loop can build software, not just text
# — is the next increment; this loop drives the richest build the busybox+make set can today.)
#
# Legs (DURABLE — no guix oracle in the room):
#   [structural]  inside, /gnu/store + /var/guix are ABSENT and guix is unresolvable.
#   [structural]  the store IS /td/store — the harness busybox lives there.
#   [behavioral]  the /td/store GNU make drives a REAL multi-target build GRAPH over the
#                 busybox userland: prerequisites, pattern rules, a deterministic artifact,
#                 and a correct INCREMENTAL rebuild (a no-op second run; a changed input
#                 rebuilds only its path). This is the loop's core operation, guix-free.
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
mv=`make --version 2>/dev/null | sed -n '1p'`
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
grep -q 'Nothing to be done\|is up to date' "$work/log2" \
  || fail "harness make: a no-op rebuild was not detected (incremental logic broken): `cat "$work/log2"`"
echo "  MAKE-INCREMENTAL-NOOP-OK (make rebuilt nothing on an unchanged tree)"

# 3. Incremental change — touch ONE input; only its path (a.up) and the report rebuild.
printf 'delta\n' > "$work/src/a.txt"
run_make >"$work/log3" 2>&1 || { cat "$work/log3" >&2; fail "harness make: incremental build failed"; }
got=`cat "$work/build/report.txt"`
want=`printf 'DELTA\nBETA\nGAMMA\n'`
[ "$got" = "$want" ] || fail "harness make: report wrong after changing a.txt (got '$got')"
# The rebuild signature is a chapter's `tr` recipe READING its src/*.txt; the report's `cat`
# line lists build/b.up build/c.up as prerequisites but that is not a rebuild of b/c.
grep -q 'src/b.txt\|src/c.txt' "$work/log3" && fail "harness make: rebuilt an UNCHANGED chapter (b/c) — incremental tracking is wrong: `cat "$work/log3"`"
echo "  MAKE-INCREMENTAL-REBUILD-OK (changed a.txt → report=DELTA/BETA/GAMMA; b,c not rebuilt)"

echo "HARNESS-LOOP-OK"
