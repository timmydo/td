#!/bin/sh
# warm-subst.sh — host-side host-prep for the toolchain FETCH short-circuit (x64-toolchain-subst,
# human 2026-06-28). If a prior DAILY run populated a persistent signed substitute store
# (~/.td/subst: a stashed td-subst binary + the published closure narinfos), ECHO the
# `export TD_SUBST_*` lines so check.sh's prelude exposes them to the loop sandbox (host-sandbox
# binds ~/.td/subst ro + preserves TD_SUBST_*). The toolchain gates then FETCH the lock-keyed
# closure instead of rebuilding ~98 min from seed, FALLING BACK to from-seed on ANY miss.
#
# This NEVER fetches or builds td-subst (that would re-introduce the ts-eval/build-recipes cascade in
# the per-PR prelude). The DAILY (which has td-subst from build-recipes) is the sole producer: it
# builds the closure from seed, signs + publishes it, and stashes its td-subst binary here. A COLD
# machine (no prior daily) → echo nothing → the gate builds from seed. Idempotent, near-instant.
set -eu

store="${TD_SUBST_STORE:-$HOME/.td/subst}"
bin="$store/td-subst"
root=$(cd "$(dirname "$0")/.." && pwd)
pub="$root/tests/td-subst.pub"

# A USABLE store = the daily's stashed td-subst binary + at least one signed narinfo + the pinned
# trust anchor. Any missing piece → no-op (the gate builds from seed; the substitute is an
# optimization, never a correctness dependency).
[ -x "$bin" ] || exit 0
ls "$store"/*.narinfo >/dev/null 2>&1 || exit 0
[ -s "$pub" ] || exit 0

printf 'export TD_SUBST_BIN=%s\n' "$bin"
printf 'export TD_SUBST_STORE=%s\n' "$store"
printf 'export TD_SUBST_PUBKEY=%s\n' "$pub"
