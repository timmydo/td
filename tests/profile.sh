#!/bin/sh
# tests/profile.sh — the user-package-manager PROFILE layer: td builds packages into a
# persistent store and `td-builder profile` unions their bin/sbin into a symlink-tree
# profile, the way a user PM works (build into ~/.td/store, link ~/.td/profile/bin/xyz ->
# store, put it on PATH / link ~/bin/xyz). This turns the seed/`td shell` build engine into
# an inspectable, durable install: not an ephemeral PATH, a real profile that runs.
#
# This gate: td BUILDS hello + which (td-builder build-recipe — no guix process, stage0
# builder), PLACES each into a persistent store (`$store/<hash>-<name>`), builds a profile
# unioning them, and runs `$profile/bin/{hello,which}` + a `~/bin`-style symlink into it.
# guix/Guile are scrubbed from the build PATH; td-builder is the guix-free stage0.
#
# Legs:
#   [DURABLE behavioral] the binaries run THROUGH the profile (and a ~/bin symlink to it)
#   [DURABLE structural] profile/bin/<tool> are symlinks INTO the persistent store (the union)
#   [DURABLE discriminate] a name provided by two packages is a detected COLLISION
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }

. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
load_recipe_eval || fail "no td-built td-recipe-eval (the build-recipes prelude must run first)"
TD_TSDIR=tests/ts
echo ">> td-builder under test (stage0, guix-free): $TB"

work=`mktemp -d`
trap 'chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"' EXIT INT TERM
mkdir -p "$work/tmp"
cu=`grep -- '-coreutils-' tests/hello-no-guix.lock | sed 's/^[^ ]* //' | head -1`

# Build a leaf recipe with td-builder (no guix process); print its td store output dir.
build_pkg() {
  _s="$1"
  sh tests/recipe-emit.sh $_s > "$work/$_s.json" || fail "ts-emit $_s"
  mkdir -p "$work/$_s-b"
  env -i HOME="$work" TMPDIR="$work/tmp" PATH="$cu/bin" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    TD_RECIPE_EVAL="$TD_RECIPE_EVAL" \
    "$TB" build-recipe "$work/$_s.json" "tests/$_s-no-guix.lock" "$work/$_s-b" /var/guix/db/db.sqlite \
    > "$work/$_s.out" 2>"$work/$_s.err" || { tail -15 "$work/$_s.err" >&2; fail "build $_s"; }
  _o=`sed -n 's/^OUT=out //p' "$work/$_s.out"`
  test -n "$_o" || fail "$_s produced no output"
  echo "$work/$_s-b/newstore/`basename "$_o"`"
}

# A PERSISTENT store (the ~/.td/store of a user PM): place each td build at $store/<base>.
store="$work/td-store"; mkdir -p "$store"
hout=`build_pkg hello`; hbase=`basename "$hout"`; cp -a "$hout" "$store/$hbase"
wout=`build_pkg which`; wbase=`basename "$wout"`; cp -a "$wout" "$store/$wbase"
echo "   td built hello + which into the persistent store: $store"

# Build the PROFILE — union their bin/ into a symlink tree.
prof="$work/profile"
"$TB" profile "$prof" "$store/$hbase" "$store/$wbase" >/dev/null || fail "td-builder profile failed"

# --- Leg A: DURABLE behavioral — run THROUGH the profile -----------------------
test "`"$prof/bin/hello"`" = "Hello, world!" || fail "$prof/bin/hello did not greet"
"$prof/bin/which" --version 2>&1 | grep -qi 'GNU which' || fail "$prof/bin/which is not GNU which"
echo "   [DURABLE behavioral] hello + which run through the profile ($prof/bin/*)"
# the ~/bin/xyz -> profile -> store chain a user PM exposes
mkdir -p "$work/bin"; ln -s "$prof/bin/hello" "$work/bin/hello"
test "`"$work/bin/hello"`" = "Hello, world!" || fail "~/bin/hello -> profile chain did not run"
echo "   [DURABLE behavioral] ~/bin/hello -> profile -> store runs (the user-PM symlink chain)"

# --- Leg B: DURABLE structural — symlinks INTO the persistent store, union -----
test -L "$prof/bin/hello" -a -L "$prof/bin/which" || fail "profile entries are not symlinks"
test "`readlink "$prof/bin/hello"`" = "$store/$hbase/bin/hello" || fail "hello symlink does not point into the store"
test "`readlink "$prof/bin/which"`" = "$store/$wbase/bin/which" || fail "which symlink does not point into the store"
echo "   [DURABLE structural] profile/bin/{hello,which} are symlinks into the persistent store (the union of both packages)"

# --- Leg C: DURABLE discriminate — a name from two packages is a collision -----
mkdir -p "$store/dup-hello/bin"; cp "$store/$hbase/bin/hello" "$store/dup-hello/bin/hello"
if "$TB" profile "$work/p2" "$store/$hbase" "$store/dup-hello" >/dev/null 2>"$work/cerr"; then
  fail "td-builder profile did NOT detect the hello collision"
fi
grep -q "collision" "$work/cerr" || { cat "$work/cerr" >&2; fail "collision not reported as a collision"; }
echo "   [DURABLE discriminate] a name provided by two packages is rejected as a collision"

echo "PASS: td built hello + which into a persistent store and td-builder profile unioned them"
echo "      into a symlink-tree profile — the binaries run through profile/bin and a ~/bin"
echo "      symlink into it; collisions are rejected. The user-package-manager profile layer."
