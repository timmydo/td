#!/bin/sh
# tests/chain-cache.sh — the chain-cache gate: the #317 FLIPPED gate-state default is
# correct. Feature under test: a Shared gate's brick builds ONCE (machine-wide), every
# reuse is NAR-verified, a poisoned cache entry is REJECTED and rebuilt (never consumed),
# and a cold run (what the runner wires for a Private gate: TD_CHAIN_CACHE="") neither
# reads nor writes the cache. Driven through the REAL library the bootstrap chain sources
# (tests/chain-cache-lib.sh) with the REAL verifier (the stage0 td-builder's nar-hash) —
# the same entry points bootstrap_modern_toolchain uses per brick.
set -eu
ROOT=$(pwd)
fail() { echo "FAIL: $*" >&2; exit 1; }

# The real verifier: the guix-free stage0 td-builder (the binary the chain gates use).
. tests/cache-lib.sh
load_stage0 || fail "stage0-builder could not place a stage0 td-builder"

work=`mktemp -d`; trap 'rm -rf "$work"' EXIT INT TERM
export TD_CHAIN_CACHE="$work/cache"
mkdir -p "$TD_CHAIN_CACHE"

. tests/chain-cache-lib.sh

# A small REAL brick: "builds" a product from pinned content, counting invocations —
# the observable that distinguishes a rebuild from a reuse.
counter="$work/builds"; : > "$counter"
build_demo() { # $1 = brick dir
  echo x >> "$counter"
  rm -rf "$1"; mkdir -p "$1/out"
  printf 'demo-product-v1\n' > "$1/out/product"
}

# One warm consumer pass — exactly the per-brick shape bootstrap_modern_toolchain runs.
pass() {
  chain_cache_init demo tests/chain-cache.sh
  test "$CHAIN_WARM" = 1 || fail "warm cache expected but the lib went cold (flock/\$TB missing?)"
  if chain_hit demo; then D=$CHAIN_PATH; else
    D="$CHAIN_DIR/demobuild"; build_demo "$D"
    chain_save demo "$D" "$D/out"
  fi
  chain_done
}

# [warm miss] the first run builds and records the brick.
pass
test "`wc -l < "$counter"`" -eq 1 || fail "first run must build exactly once"
test -f "$D/out/product" || fail "no product after the first build"

# [warm hit] the second run reuses the NAR-verified brick — the build must NOT run again.
pass
test "`wc -l < "$counter"`" -eq 1 || fail "second run must cache-hit, but the build ran again"

# [poison → REJECT] tamper with the cached product: reuse must be REFUSED (nar mismatch),
# the brick torn down and rebuilt — sharing never means trusting bytes on presence.
printf 'tampered\n' > "$D/out/product"
pass
test "`wc -l < "$counter"`" -eq 2 || fail "poisoned cache entry was consumed without a rebuild"
grep -q 'demo-product-v1' "$D/out/product" || fail "the rebuild did not restore the product"

# [cold / Private wiring] TD_CHAIN_CACHE empty — the runner's Private setting: the cache is
# neither read (the poisoned-shared-state differential: a cold gate must not see warm
# state) nor written.
sent_before=`ls "$TD_CHAIN_CACHE"/demo-*/.brick-demo 2>/dev/null | wc -l`
warm_cache="$TD_CHAIN_CACHE"
TD_CHAIN_CACHE=
chain_cache_init demo tests/chain-cache.sh
test "$CHAIN_WARM" = 0 || fail "cold run must not go warm"
if chain_hit demo; then fail "cold run must never hit the cache"; fi
CD="$work/coldbuild"; build_demo "$CD"
chain_save demo "$CD" "$CD/out"
test "`wc -l < "$counter"`" -eq 3 || fail "cold run must build from scratch"
TD_CHAIN_CACHE="$warm_cache"
sent_after=`ls "$TD_CHAIN_CACHE"/demo-*/.brick-demo 2>/dev/null | wc -l`
test "$sent_after" = "$sent_before" || fail "cold run wrote the warm cache"

echo "PASS: chain-cache — warm builds once, reuse is NAR-verified, a poisoned entry is rejected+rebuilt, cold never touches the cache"
