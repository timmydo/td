#!/bin/sh
# tests/toolchain-subst-default.sh — gate: the loop FETCHES the lock-keyed /td/store
# toolchain by DEFAULT (tools/resolve-toolchain.sh) instead of rebuilding the ~18-rung
# from-seed chain. "Loop substitutes too" (human, 2026-06-28). The genuinely new bits vs
# gate 358 (which uses ephemeral served dirs + ephemeral keys inline): a PERSISTENT signed
# substitute store keyed by tests/td-toolchain.lock and the consumer-DEFAULT resolver
# script that a real bootstrap gate sources — fetch-by-default, FALL BACK to from-seed on
# any miss.
#
# DELIBERATE directive-1 relaxation (human-approved, surfaced here): with the resolver the
# per-PR/local loop no longer builds the toolchain from source; ci/daily-full-suite.sh on
# fresh main is the SOLE remaining from-seed authoritative build + the publisher.
#
#   [DURABLE behavioral] DEFAULT FETCH: a real runnable artifact is interned at the
#     input-addressed path `toolchain-path tests/td-toolchain.lock glibc-2.41`, exported +
#     signed into a PERSISTENT store; the resolver (given ONLY the lock + the store + the
#     pinned pub) computes that path, fetches it (sig + StorePath + NarHash verified),
#     restores it, and the caller RUNS the fetched-not-built binary -> a toolchain path
#     obtained WITHOUT building it, by default.
#   [DURABLE behavioral] FALL BACK: an empty/cold store -> the resolver MISSES (exit 1, no
#     path) so the caller builds from seed. The substitute is an optimization, never a
#     correctness dependency.
#   [SELF-DISCRIMINATION] a WRONG pinned key -> the resolver's fetch is rejected -> MISS ->
#     fall back. The ed25519 signature (pinned key) is load-bearing.
#   [DURABLE structural] tests/td-subst.pub is a well-formed 32-byte ed25519 anchor (the
#     production trust anchor the daily-suite publisher signs against).
# Trust = signature + the input-addressed NAME, NOT repro-equality (the toolchain is not
# byte-reproducible; that is task 3). The subst binary is td-BUILT from source (move-off-
# Guile §5), reusing tests/td-subst.lock exactly like gate 358.
set -eu
cd "$(dirname "$0")/.."

tsgo=$(sh tests/tsgo.sh)
test -n "$tsgo" -a -x "$tsgo/lib/tsc" || { echo "ERROR: could not resolve td-tsgo" >&2; exit 1; }
. tests/cache-lib.sh
export TD_STAGE0_BASE="$(pwd)/.td-build-cache/stage0"
load_stage0; load_ts_eval; tb="$TB"
export TD_TSGO="$tsgo" TD_TSDIR="$(pwd)/tests/ts"

# --- build td-subst from source (its own cache dir; CACHE=hit on reruns) ---
lock0="$(pwd)/tests/td-subst.lock"
test -s "$lock0" || { echo "ERROR: no $lock0" >&2; exit 1; }
cu=$(grep -- '-coreutils-' "$lock0" | sed 's/^[^ ]* //' | head -1)
test -n "$cu" || { echo "ERROR: no coreutils in the lock" >&2; exit 1; }
scratch="$(pwd)/.td-build-cache/toolchain-subst-default"; mkdir -p "$scratch/tmp" "$scratch/b"; rm -f "$scratch/b/"*.drv
grep ' /gnu/store/' "$lock0" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null \
  || { echo "ERROR: could not realize the seed + vendored .crate deps" >&2; exit 1; }
srcinfo=$(sh tests/intern-src.sh "$tb" td-subst-src "$(pwd)/subst" "$scratch" target vendor .cargo) \
  || { echo "ERROR: could not intern the subst crate tree" >&2; exit 1; }
eval "$srcinfo"
lock="$scratch/td-subst.lock"; { cat "$lock0"; echo "td-subst-source $src"; } > "$lock"
sh tests/ts-emit.sh "$(pwd)/tests/ts/recipe-td-subst.ts" > "$scratch/subst.json"
test -s "$scratch/subst.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }
sd="$scratch/b"
env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$cu/bin" \
  TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
  "$tb" build-recipe "$scratch/subst.json" "$lock" "$sd" /var/guix/db/db.sqlite "$srcstore" "$srcdb" \
  > "$scratch/bout" 2>"$scratch/err" || { echo "FAIL: build-recipe td-subst:" >&2; tail -20 "$scratch/err" >&2; exit 1; }
out=$(sed -n 's/^OUT=out //p' "$scratch/bout")
ts="$sd/newstore/$(basename "$out")/bin/td-subst"
test -x "$ts" || { echo "FAIL: no td-subst binary at $ts" >&2; exit 1; }
echo "  [DURABLE structural] td-built td-subst from source (move-off-Guile §5): $out"

# --- producer: intern a runnable fixture at the lock-keyed path, export, sign (ephemeral
#     key — CI has no production private secret), into a PERSISTENT store ---
ttl="$(pwd)/tests/td-toolchain.lock"; test -s "$ttl" || { echo "FAIL: no td-toolchain.lock" >&2; exit 1; }
key=$(env -i PATH="$cu/bin" "$tb" toolchain-key "$ttl")
test -n "$key" || { echo "FAIL: toolchain-key produced nothing" >&2; exit 1; }
# a real static bash from hello's pinned closure (runs directly, no interp) as the fixture
bashpkg=$(grep -- '-bash-' "$(pwd)/tests/hello-no-guix.lock" | grep -v static | sed 's/^[^ ]* //' | head -1)
fixt=$(env -i PATH="$cu/bin" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
       "$tb" store-closure /var/guix/db/db.sqlite "$bashpkg" | grep -- '-bash-static-' | head -1)
test -n "$fixt" -a -x "$fixt/bin/bash" || { echo "FAIL: no static bash fixture in hello's closure" >&2; exit 1; }
W="$scratch/ia"; rm -rf "$W"; mkdir -p "$W/phys" "$W/store" "$W/dest"
path=$(env -i PATH="$cu/bin" TD_STORE_DIR=/td/store "$tb" store-add-input-addressed glibc-2.41 "$key" "$fixt" "$W/phys" "$W/td.db")
base=$(basename "$path")
case "$path" in /td/store/*-glibc-2.41) : ;; *) echo "FAIL: not input-addressed at /td/store: $path" >&2; exit 1 ;; esac
env -i PATH="$cu/bin" "$tb" subst-export "$W/td.db" "$W/phys" "$W/store" "$path" >/dev/null
test -f "$W/store/$base.narinfo" || { echo "FAIL: no narinfo exported for $base" >&2; exit 1; }
"$ts" keygen "$W/priv" "$W/pub" >/dev/null
"$ts" sign "$W/store" "$W/priv" >/dev/null
grep -q '^Sig: ' "$W/store/$base.narinfo" || { echo "FAIL: sign did not sign the narinfo" >&2; exit 1; }

# --- [DURABLE behavioral] DEFAULT FETCH: the resolver fetches+verifies+restores, no build ---
got=$(env -i PATH="$cu/bin" TD_SUBST_BIN="$ts" TD_BUILDER="$tb" TD_SUBST_STORE="$W/store" \
      TD_SUBST_PUBKEY="$W/pub" TD_STORE_DIR=/td/store sh tools/resolve-toolchain.sh "$ttl" glibc-2.41 "$W/dest")
test "x$got" = "x$W/dest/$base" || { echo "FAIL: resolver did not print the restored path (got '$got')" >&2; exit 1; }
ran=$(env -i "$got/bin/bash" -c 'echo RAN-FETCHED')
test "x$ran" = "xRAN-FETCHED" || { echo "FAIL: the fetched (not built) binary did not run (got '$ran')" >&2; exit 1; }
echo "  [DURABLE behavioral] DEFAULT FETCH: the resolver computed the lock-keyed path, fetched it (sig + StorePath + NarHash verified) and the fetched-not-built binary RAN -> $ran"

# --- [DURABLE behavioral] FALL BACK on a cold store ---
mkdir -p "$W/empty"
if env -i PATH="$cu/bin" TD_SUBST_BIN="$ts" TD_BUILDER="$tb" TD_SUBST_STORE="$W/empty" \
   TD_SUBST_PUBKEY="$W/pub" TD_STORE_DIR=/td/store sh tools/resolve-toolchain.sh "$ttl" glibc-2.41 "$W/d2" >/dev/null 2>&1; then
  echo "FAIL: resolver returned 0 on a COLD store (should MISS -> fall back)" >&2; exit 1
fi
echo "  [DURABLE behavioral] FALL BACK: a cold store -> the resolver MISSES (exit 1, no path) -> the caller builds from seed"

# --- [SELF-DISCRIMINATION] a WRONG pinned key -> rejected -> MISS ---
"$ts" keygen "$W/wrong.priv" "$W/wrong.pub" >/dev/null
if env -i PATH="$cu/bin" TD_SUBST_BIN="$ts" TD_BUILDER="$tb" TD_SUBST_STORE="$W/store" \
   TD_SUBST_PUBKEY="$W/wrong.pub" TD_STORE_DIR=/td/store sh tools/resolve-toolchain.sh "$ttl" glibc-2.41 "$W/d3" >/dev/null 2>&1; then
  echo "FAIL: resolver ACCEPTED a substitute under a WRONG pinned key (signature not load-bearing)" >&2; exit 1
fi
echo "  [SELF-DISCRIMINATION] a wrong pinned key -> the resolver's fetch is rejected -> MISS -> fall back (signature load-bearing)"

# --- [SELF-DISCRIMINATION] a VALIDLY-SIGNED narinfo for a DIFFERENT StorePath (re-signed)
#     -> the resolver's own StorePath==lock-path check rejects it (td-subst fetch verifies
#     sig + NarHash but not that the path is the one we asked for) ---
cp -r "$W/store" "$W/store2"
sed -i 's#^StorePath: .*#StorePath: /td/store/00000000000000000000000000000000-glibc-2.41#' "$W/store2/$base.narinfo"
"$ts" sign "$W/store2" "$W/priv" >/dev/null
if env -i PATH="$cu/bin" TD_SUBST_BIN="$ts" TD_BUILDER="$tb" TD_SUBST_STORE="$W/store2" \
   TD_SUBST_PUBKEY="$W/pub" TD_STORE_DIR=/td/store sh tools/resolve-toolchain.sh "$ttl" glibc-2.41 "$W/d4" >/dev/null 2>&1; then
  echo "FAIL: resolver ACCEPTED a validly-signed substitute whose StorePath != the lock-computed path" >&2; exit 1
fi
echo "  [SELF-DISCRIMINATION] a validly-signed narinfo for a DIFFERENT StorePath -> resolver MISS (the input-addressed name is load-bearing alongside the signature)"

# --- [DURABLE structural] the pinned production anchor is well-formed ---
h=$(tr -d '\n' < tests/td-subst.pub)
case "$h" in *[!0-9a-f]*) echo "FAIL: tests/td-subst.pub has non-hex chars" >&2; exit 1 ;; esac
test "$(printf %s "$h" | wc -c)" -eq 64 || { echo "FAIL: tests/td-subst.pub is not a 32-byte ed25519 key" >&2; exit 1; }
echo "  [DURABLE structural] tests/td-subst.pub is a well-formed 32-byte ed25519 trust anchor"

rm -rf "$W" "$scratch/tmp" "$scratch/bout" "$scratch/err"; mkdir -p "$scratch/tmp"
echo "PASS: the loop resolves the lock-keyed /td/store toolchain by DEFAULT — tools/resolve-toolchain.sh computes the input-addressed path from tests/td-toolchain.lock, fetches the signed substitute from a persistent store (ed25519 sig vs the pinned key + StorePath == the lock-computed path + NarHash verified) and RUNS the fetched-not-built artifact, FALLS BACK to the from-seed build on a cold store, and reds on a wrong pinned key (durable behavioral + self-discrimination). Deliberate directive-1 relaxation: the daily full suite is the sole from-seed authoritative build + the publisher."
