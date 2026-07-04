#!/bin/sh
# tools/publish-seed-subst.sh — the PRODUCER half of #311 (seed realizations via
# td-subst): export the pinned /gnu/store SEED closure as SIGNED substitutes so the
# loop's seed resolver (tools/resolve-seed.sh) can realize a missing seed WITHOUT a
# guix process. Runs in ci/daily-full-suite.sh's all-green publish block, next to
# publish-toolchain-subst.sh; the gate (tests/seed-subst.sh) drives it against a
# scratch store with an ephemeral key.
#
# The closure is captured CONTENT-SCANNED from the live store bytes (tools/warm-seed.sh
# with TD_SEED_DB=<store dir> — zero reads of guix's private db, directive 6) into a
# NAR-verified td-owned copy, subst-exported per member (each narinfo's References
# carry the closure edges the resolver walks), signed with the runner's private key
# (trust anchor: tests/td-subst.pub), and copied into the persistent substitute store.
#
# IDEMPOTENT: when every lock root already has a narinfo in OUT_STORE it exits 0
# without re-exporting — the seed only changes on a channel bump, which changes the
# basenames and so re-triggers the export.
#
# Usage: publish-seed-subst.sh LOCK OUT_STORE
# Env:
#   TD_BUILDER        td-builder (seed capture + subst-export) (REQUIRED)
#   TD_SUBST_BIN      td-subst (sign) (REQUIRED)
#   TD_SUBST_PRIVKEY  the runner's ed25519 private key, pkcs8 (REQUIRED, host secret)
#   TD_SEED_SRC       the store dir the seed bytes live in (default /gnu/store)
#   TD_SEED_WARM      warm-seed cache base (default .td-build-cache/seed-subst-warm)
set -eu

lock=${1:?usage: publish-seed-subst.sh LOCK OUT_STORE}
out=${2:?usage: publish-seed-subst.sh LOCK OUT_STORE}
: "${TD_BUILDER:?TD_BUILDER unset}"
: "${TD_SUBST_BIN:?TD_SUBST_BIN unset}"
: "${TD_SUBST_PRIVKEY:?TD_SUBST_PRIVKEY unset (the runner private key is a host secret)}"
src=${TD_SEED_SRC:-/gnu/store}
warmbase=${TD_SEED_WARM:-.td-build-cache/seed-subst-warm}

roots=$(sed -n 's/^[^ ]* \(\/gnu\/store\/[^ ]*\)$/\1/p' "$lock" 2>/dev/null) || roots=""
[ -n "$roots" ] || { echo "publish-seed-subst: no /gnu/store seed paths in $lock" >&2; exit 1; }

# Idempotence: the whole published CLOSURE must be complete -- every root narinfo, every
# member narinfo named in the References (walked from the local files), and every named
# nar. A publish interrupted mid-copy, or a store that lost members, re-triggers the
# export instead of wedging at nothing-to-do while cold hosts fail on a missing member.
published_complete() {
  _q=""
  for _p in $roots; do _q="$_q ${_p##*/}"; done
  _s=" "
  while :; do
    _q="${_q# }"
    [ -n "$_q" ] || return 0
    case "$_q" in *" "*) _b="${_q%% *}"; _q="${_q#* }" ;; *) _b="$_q"; _q="" ;; esac
    case "$_s" in *" $_b "*) continue ;; esac
    _s="$_s$_b "
    _ni="$out/$_b.narinfo"
    [ -f "$_ni" ] || return 1
    _nf=$(sed -n 's/^NarFile: //p' "$_ni")
    { [ -n "$_nf" ] && [ -f "$out/$_nf" ]; } || return 1
    _r=$(sed -n 's/^References: //p' "$_ni")
    [ -n "$_r" ] && _q="$_q $_r"
  done
}
if published_complete; then
  echo "publish-seed-subst: the full closure of $lock is already published in $out — nothing to do"
  exit 0
fi

for p in $roots; do
  [ -e "$src/${p##*/}" ] \
    || { echo "publish-seed-subst: seed root $p not present under $src — nothing to capture" >&2; exit 1; }
done

# Content-scanned, NAR-verified closure copy (store + refs db) — warm-seed caches it by
# root set, so a re-publish after a partial store wipe reuses the capture.
# shellcheck disable=SC2086 -- $roots is a whitespace-separated store-path list on purpose
wm=$(TB="$TD_BUILDER" TD_SEED_DB="$src" sh "$(dirname "$0")/warm-seed.sh" "$warmbase" $roots) \
  || { echo "publish-seed-subst: seed capture (warm-seed) failed" >&2; exit 1; }
# shellcheck disable=SC2086 -- warm-seed prints the triple: store db manifest
set -- $wm
seedstore=$1; seeddb=$2

exp="$warmbase/export.$$"
rm -rf "$exp"; mkdir -p "$exp"
trap 'rm -rf "$exp"' EXIT
# shellcheck disable=SC2086 -- $roots again a deliberate list
"$TD_BUILDER" subst-export "$seeddb" "$seedstore" "$exp" $roots >/dev/null \
  || { echo "publish-seed-subst: subst-export failed" >&2; exit 1; }
"$TD_SUBST_BIN" sign "$exp" "$TD_SUBST_PRIVKEY" >/dev/null \
  || { echo "publish-seed-subst: sign failed" >&2; exit 1; }
for p in $roots; do
  grep -q '^Sig: ' "$exp/${p##*/}.narinfo" \
    || { echo "publish-seed-subst: ${p##*/}.narinfo not signed" >&2; exit 1; }
done
# nars BEFORE narinfos: a narinfo must never be visible without its nar, so an
# interrupted copy is re-triggered by the completeness walk above, never half-served.
mkdir -p "$out/nar"
cp -a "$exp"/nar/. "$out"/nar/
cp -a "$exp"/*.narinfo "$out"/
n=$(ls "$exp"/*.narinfo | grep -c . || true)
echo "publish-seed-subst: published $n narinfo(s) — the signed $lock seed closure -> $out"
