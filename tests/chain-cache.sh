#!/bin/sh
# tests/chain-cache.sh — the chain-cache gate: the #317 FLIPPED gate-state default is
# correct. Feature under test: a Shared gate's brick builds ONCE (machine-wide), every
# reuse is NAR-verified, a poisoned cache entry is REJECTED and rebuilt (never consumed),
# and a cold run (what the runner wires for a Private gate: TD_CHECK_CHAIN_CACHE="") neither
# reads nor writes the cache. Plus whole-key GC (#326): chain_cache_init sweeps a stale key
# (last-used aged past the threshold) while the live key still NAR-verifies + cache-hits,
# and a key whose exclusive lock is held (a concurrent build) is NEVER swept. Driven
# through the REAL library the bootstrap chain sources (tests/chain-cache-lib.sh) with the
# REAL verifier (the stage0 td-builder's nar-hash) — the same entry points
# bootstrap_modern_toolchain uses per brick.
set -eu
ROOT=$(pwd)
fail() { echo "FAIL: $*" >&2; exit 1; }

# The real verifier: the guix-free stage0 td-builder (the binary the chain gates use).
. tests/cache-lib.sh
load_stage0 || fail "stage0-builder could not place a stage0 td-builder"

work=`mktemp -d`; trap 'rm -rf "$work"' EXIT INT TERM
export TD_CHECK_CHAIN_CACHE="$work/cache"
mkdir -p "$TD_CHECK_CHAIN_CACHE"

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
  test "$CHAIN_WARM" = 1 || fail "warm cache expected but the lib went cold (lock helper/\$TB missing?)"
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

# [cold / Private wiring] TD_CHECK_CHAIN_CACHE empty — the runner's Private setting: the cache is
# neither read (the poisoned-shared-state differential: a cold gate must not see warm
# state) nor written.
sent_before=`ls "$TD_CHECK_CHAIN_CACHE"/demo-*/.brick-demo 2>/dev/null | wc -l`
warm_cache="$TD_CHECK_CHAIN_CACHE"
TD_CHECK_CHAIN_CACHE=
chain_cache_init demo tests/chain-cache.sh
test "$CHAIN_WARM" = 0 || fail "cold run must not go warm"
if chain_hit demo; then fail "cold run must never hit the cache"; fi
CD="$work/coldbuild"; build_demo "$CD"
chain_save demo "$CD" "$CD/out"
test "`wc -l < "$counter"`" -eq 3 || fail "cold run must build from scratch"
TD_CHECK_CHAIN_CACHE="$warm_cache"
sent_after=`ls "$TD_CHECK_CHAIN_CACHE"/demo-*/.brick-demo 2>/dev/null | wc -l`
test "$sent_after" = "$sent_before" || fail "cold run wrote the warm cache"

# [whole-key GC] (#326) A stream of recipe/pin/channel changes mints a fresh multi-GB key
# per tuple and nothing prunes inside a key, so reclamation must be whole-key. Populate
# three keys under one namespace: LIVE (fresh), STALE (stamp aged past the threshold), and
# HELD (also aged, but a concurrent build holds its exclusive .lock). Re-initing the LIVE
# key must sweep STALE, spare HELD, and leave LIVE cache-hitting — never trusting presence.
gckey() { f="$work/gc-$1.key"; printf '%s\n' "$1" > "$f"; echo "$f"; }
# populate KEYFILE — build+record the key's brick once, release the lock; sets POP_DIR to
# the key dir. Runs in THIS shell (not a subshell) so chain_cache_init's fd-9 lock, CHAIN_DIR,
# and chain_done stay coherent in the parent.
populate() {
  chain_cache_init demo "$1"
  test "$CHAIN_WARM" = 1 || fail "gc: key $1 did not go warm"
  if ! chain_hit demo; then B="$CHAIN_DIR/demobuild"; build_demo "$B"; chain_save demo "$B" "$B/out"; fi
  POP_DIR="$CHAIN_DIR"
  chain_done
}

LK=`gckey live`;  populate "$LK";  LIVE_DIR="$POP_DIR"
SK=`gckey stale`; populate "$SK";  STALE_DIR="$POP_DIR"
HK=`gckey held`;  populate "$HK";  HELD_DIR="$POP_DIR"
# Age STALE and HELD past the default threshold (14d); LIVE keeps its fresh stamp.
touch -d '400 days ago' "$STALE_DIR/last-used" "$HELD_DIR/last-used"
# A concurrent agent building/reusing HELD == its exclusive .lock is held. Kernel flock locks each
# open-file-description independently (even within one process), so holding fd 7 here is a
# faithful stand-in for another agent's hold: the GC's own nonblocking lock on HELD is denied.
exec 7>>"$HELD_DIR/.lock"; chain_lock_fd -n 7 || fail "gc: could not take the simulated concurrent-build lock on HELD"

builds_before=`wc -l < "$counter"`
# Re-init LIVE: chain_cache_init re-stamps LIVE and runs the sweep.
chain_cache_init demo "$LK"
test "$CHAIN_WARM" = 1 || fail "gc: live re-init did not go warm"
chain_hit demo || fail "gc: the live key must still NAR-verify + cache-hit after GC"
chain_done
exec 7>&-   # release the simulated concurrent-build lock

test "`wc -l < "$counter"`" -eq "$builds_before" || fail "gc: the live key rebuilt instead of cache-hitting after the sweep"
test ! -d "$STALE_DIR" || fail "gc: the stale key was NOT swept ($STALE_DIR)"
test -d "$HELD_DIR" || fail "gc: a key holding the lock (concurrent build) was swept ($HELD_DIR)"
test -d "$LIVE_DIR" || fail "gc: the live key was swept ($LIVE_DIR)"

echo "PASS: chain-cache — warm builds once, reuse is NAR-verified, a poisoned entry is rejected+rebuilt, cold never touches the cache, and whole-key GC sweeps a stale key while sparing the live and the lock-held ones"
