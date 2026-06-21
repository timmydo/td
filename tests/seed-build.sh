#!/bin/sh
# tests/seed-build.sh — North-Star step 2 (CLAUDE.md), PR3 — the payoff: BUILD hello from
# the UNPACKED SEED, with NO guix install. We capture hello's full build closure (its lock
# inputs + the stage0 builder's runtime) into a frozen tarball, `seed-unpack` it into a
# FRESH td store, and then `td-builder build-recipe` builds hello passing the unpacked seed
# DB as its ONLY store DB (TD_SEED_STORE/TD_SEED_DB) — so /var/guix and the live /gnu/store
# are out of the build's input path. If any seed path were missing the build would fail (it
# cannot fall back to guix), so a green build proves the tarball is a self-sufficient seed.
#
# guix/Guile are SCRUBBED FROM PATH (no guix process); guix appears only as the one-time
# capture SOURCE (the seed comes from it once) + the removable equivalence oracle.
#
# Legs:
#   [DURABLE behavioral] hello builds from the unpacked seed (store DB = the seed only) + RUNS
#   [DURABLE structural] the build stages seed inputs FROM the unpacked store (closure binds
#                        on-disk under DEST-STORE, not /gnu/store)
#   [REMOVABLE oracle]   the seed-built hello is the SAME store path as the guix-seed build
#                        (identical drv — own, then diverge: provenance changed, output didn't)
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }

. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
load_ts_eval || fail "no td-built td-ts-eval (the build-recipes prelude must run first)"
TD_TSGO=`sh tests/tsgo.sh` || fail "could not resolve td-tsgo"
TD_TSDIR=tests/ts
export TD_TSGO TD_TSDIR
echo ">> td tools (guix-free): stage0=$TB  ts-eval=$TD_TS_EVAL"

work=`mktemp -d`
trap 'chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"' EXIT INT TERM
cu=`grep -- '-coreutils-' tests/hello-no-guix.lock | sed 's/^[^ ]* //' | head -1`
test -n "$cu" || fail "no coreutils in hello lock"

# Capture roots: hello's lock inputs (toolchain + source) + the stage0 builder's runtime
# refs (so the seed covers the builder too) — their union closure is hello's full seed.
grep ' /gnu/store/' tests/hello-no-guix.lock | sed 's/^[^ ]* //' | sort -u > "$work/roots"
"$TB" store-query "$TD_BUILDER_DB" references 2>/dev/null | sed 's/^[^|]*|//' | grep '^/gnu/store/' >> "$work/roots" || true
sort -u "$work/roots" -o "$work/roots"
grep ' /gnu/store/' tests/hello-no-guix.lock | sed 's/^[^ ]* //' | sort -u | xargs guix build >/dev/null \
  || fail "could not realize hello's seed closure"

# CAPTURE -> tar + manifest, then UNPACK into a fresh td store (no daemon).
TB="$TB" TD_SEED_DB=/var/guix/db/db.sqlite sh tools/build-seed-tarball.sh "$work/cap" `cat "$work/roots"` >/dev/null \
  || fail "build-seed-tarball failed"
ns=`grep -c . "$work/cap/seed.manifest"`
"$TB" seed-unpack "$work/cap/seed.tar" "$work/cap/seed.manifest" "$work/store" "$work/seed.db" >/dev/null \
  || fail "seed-unpack failed"
echo "   captured + unpacked hello's full seed: $ns paths (`du -h "$work/cap/seed.tar" | cut -f1`)"

# --- Leg A: DURABLE behavioral — build hello from the unpacked seed ONLY -------
sh tests/ts-emit.sh tests/ts/recipe-hello.ts > "$work/hello.json" || fail "ts-emit hello"
mkdir -p "$work/b"
env -i HOME="$work" TMPDIR="$work" PATH="$cu/bin" \
  TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
  TD_SEED_STORE="$work/store/gnu/store" TD_SEED_DB="$work/seed.db" \
  "$TB" build-recipe "$work/hello.json" tests/hello-no-guix.lock "$work/b" "$work/seed.db" \
  > "$work/out" 2>"$work/err" \
  || { tail -20 "$work/err" >&2; fail "build hello from the unpacked seed FAILED (seed not self-sufficient?)"; }
out=`sed -n 's/^OUT=out //p' "$work/out"`
test -n "$out" || fail "build-recipe produced no output"
hb="$work/b/newstore/`basename "$out"`/bin/hello"
test -x "$hb" || fail "no hello binary at $hb"
test "`"$hb"`" = "Hello, world!" || fail "the seed-built hello did not greet"
echo "   [DURABLE behavioral] hello BUILT from the unpacked seed (store DB = the seed only, /var/guix out of the path) and RAN: Hello, world!"

# --- Leg B: DURABLE structural — the build staged inputs FROM the unpacked store
test -s "$work/b/closure.txt" || fail "no closure.txt from the build"
grep -q "	$work/store/gnu/store/" "$work/b/closure.txt" \
  || fail "the build did not stage any input from the unpacked seed store ($work/store)"
# and NO bare seed input was left pointing at the live /gnu/store with no on-disk override
bare=`grep -v '	' "$work/b/closure.txt" | grep '^/gnu/store/' | grep -v "^$out$" | head -1 || true`
test -z "$bare" || fail "an input was staged from the live /gnu/store, not the seed: $bare"
echo "   [DURABLE structural] every input staged from the unpacked seed store (none from the live /gnu/store)"

# --- Leg C: REMOVABLE oracle — same store path as the guix-seed build ----------
mkdir -p "$work/g"
env -i HOME="$work" TMPDIR="$work" PATH="$cu/bin" \
  TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
  "$TB" build-recipe "$work/hello.json" tests/hello-no-guix.lock "$work/g" /var/guix/db/db.sqlite \
  > "$work/gout" 2>"$work/gerr" || { tail -10 "$work/gerr" >&2; fail "guix-seed build (oracle) failed"; }
gout=`sed -n 's/^OUT=out //p' "$work/gout"`
test "$out" = "$gout" || fail "seed-built hello ($out) != guix-seed build ($gout) — provenance changed the output"
echo "   [REMOVABLE oracle] seed-built hello == guix-seed build (`basename "$out"`) — same drv, provenance-only change"

echo "PASS: hello built entirely from the UNPACKED SEED tarball (its only store DB) and ran —"
echo "      /var/guix + the live /gnu/store out of the build path; the frozen seed is"
echo "      self-sufficient. td builds with no guix install (North-Star step 2 PR3)."
