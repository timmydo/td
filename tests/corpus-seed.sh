#!/bin/sh
# tests/corpus-seed.sh — North-Star: ONE warmed seed builds MULTIPLE corpus packages with
# no guix install. seed-build proved hello builds from the seed; this generalizes it — a
# single warmed seed (the union of two packages' build closures) builds two DIFFERENT
# corpus tools (hello + which) from source, each passing the seed DB as its ONLY store DB,
# so /var/guix + the live /gnu/store are out of every build. Proves the seed mechanism
# scales to the corpus (one seed, many builds — the shape of "the whole loop builds with no
# guix install"); chained corpus packages (build-plan seed support) are the next step.
#
# Leaf corpus recipes (no owned-input edges) build with `build-recipe`'s seed-store override
# (#133) — no code change. td-builder is the guix-free stage0; guix is only the one-time
# capture source + the removable oracle. Builds run with guix/Guile scrubbed from PATH.
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }

. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
load_recipe_eval || fail "no td-built td-recipe-eval (the build-recipes prelude must run first)"
TD_TSDIR=tests/ts
echo ">> td tools (guix-free): stage0=$TB  ts-eval=$TD_RECIPE_EVAL"

work=`mktemp -d`
trap 'chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"' EXIT INT TERM
mkdir -p "$work/tmp"
cu=`grep -- '-coreutils-' tests/hello-no-guix.lock | sed 's/^[^ ]* //' | head -1`
sh_=`grep -- '-bash-' tests/hello-no-guix.lock | sed 's/^[^ ]* //' | head -1`

SPECS="hello which"

# ONE shared seed = the union of every spec's lock inputs + the stage0 builder's runtime.
: > "$work/roots"
for s in $SPECS; do grep ' /gnu/store/' "tests/$s-no-guix.lock" | sed 's/^[^ ]* //' >> "$work/roots"; done
"$TB" store-query "$TD_BUILDER_DB" references 2>/dev/null | sed 's/^[^|]*|//' | grep '^/gnu/store/' >> "$work/roots" || true
sort -u "$work/roots" -o "$work/roots"
xargs guix build < "$work/roots" >/dev/null || fail "could not realize the seed closure"
seedline=`TB="$TB" TD_SEED_DB=/var/guix/db/db.sqlite sh tools/warm-seed.sh "$(pwd)/.td-build-cache/seed" $(cat "$work/roots")` \
  || fail "warm-seed failed"
SEED_STORE=`echo "$seedline" | cut -d' ' -f1`; SEED_DB=`echo "$seedline" | cut -d' ' -f2`
test -d "$SEED_STORE" -a -s "$SEED_DB" || fail "warm-seed produced no usable seed"
echo "   one shared warmed seed (`grep -c . "$work/roots"` roots): $SEED_STORE"

# Build a leaf corpus spec from the shared seed; print its output dir.
build_from_seed() {
  _s="$1"
  sh tests/recipe-emit.sh $_s > "$work/$_s.json" || fail "ts-emit $_s"
  mkdir -p "$work/$_s-b"
  env -i HOME="$work" TMPDIR="$work/tmp" PATH="$cu/bin:$sh_/bin" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    TD_RECIPE_EVAL="$TD_RECIPE_EVAL" \
    TD_SEED_STORE="$SEED_STORE" TD_SEED_DB="$SEED_DB" \
    "$TB" build-recipe "$work/$_s.json" "tests/$_s-no-guix.lock" "$work/$_s-b" "$SEED_DB" \
    > "$work/$_s.out" 2>"$work/$_s.err" || { tail -15 "$work/$_s.err" >&2; fail "build $_s from the seed"; }
  _o=`sed -n 's/^OUT=out //p' "$work/$_s.out"`
  test -n "$_o" || fail "$_s produced no output"
  # every input staged FROM the seed store (none bare from the live /gnu/store)
  _bare=`grep -v '	' "$work/$_s-b/closure.txt" | grep '^/gnu/store/' | grep -v "/$(basename "$_o")\$" | head -1 || true`
  test -z "$_bare" || fail "$_s staged an input from the live /gnu/store, not the seed: $_bare"
  echo "$work/$_s-b/newstore/`basename "$_o"`"
}

# --- hello from the shared seed ---
hd=`build_from_seed hello`
test "`"$hd/bin/hello"`" = "Hello, world!" || fail "seed-built hello did not greet"
echo "   [DURABLE] hello built from the shared seed (no guix) and greeted"

# --- which from the SAME shared seed (a different corpus tool) ---
wd=`build_from_seed which`
"$wd/bin/which" --version 2>&1 | grep -qi 'GNU which' || fail "seed-built which --version is not GNU which"
echo "   [DURABLE] which built from the SAME shared seed (no guix) and runs: `"$wd/bin/which" --version 2>&1 | head -1`"

echo "PASS: ONE warmed seed built TWO different corpus packages (hello + which) from source —"
echo "      each with the seed as its only store DB (/var/guix + the live /gnu/store out of"
echo "      the build), every input staged from the seed. The seed scales to the corpus: one"
echo "      seed, many builds, no guix install (leaf recipes; chained is build-plan-seed next)."
