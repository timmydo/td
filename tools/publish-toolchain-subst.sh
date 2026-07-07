#!/bin/sh
# tools/publish-toolchain-subst.sh — the PRODUCER half of "loop substitutes too"
# (human, 2026-06-28): publish the lock-keyed /td/store toolchain as a SIGNED substitute
# so the loop's resolver (tools/resolve-toolchain.sh) fetches it instead of rebuilding the
# ~18-rung from-seed chain.
#
# Runs in td-builder daily AFTER the authoritative from-seed build interns the
# toolchain at the input-addressed path (gate 412's store-add-input-addressed). It exports
# that path as a NAR + a td-native narinfo and signs the narinfo with the daily-runner's
# PRIVATE key (whose public half is the pinned tests/td-subst.pub the loop verifies). The
# daily from-seed build remains the authoritative build AND the provenance of these bytes;
# this only packages them for the cache. Trust = the ed25519 signature + the input-addressed
# NAME (the toolchain is not byte-reproducible; repro-equality is task 3).
#
# Usage: tools/publish-toolchain-subst.sh LOCK NAME DB STORE_DIR OUT_STORE
#   LOCK       tests/td-toolchain.lock (derives the input-addressed key/path)
#   NAME       the component to publish (e.g. glibc-2.41)
#   DB         the store db that records the interned path's StorePath (logical /td/store)
#   STORE_DIR  the physical store dir holding the interned bytes
#   OUT_STORE  the persistent substitute store dir to write <hash>.narinfo + nar/ into
# Env:
#   TD_BUILDER        td-builder (toolchain-path/subst-export) (REQUIRED)
#   TD_SUBST_BIN      td-subst (sign) (REQUIRED)
#   TD_SUBST_PRIVKEY  the daily-runner's ed25519 private key, pkcs8 (REQUIRED, host secret)
#   TD_STORE_DIR      logical store prefix (default: /td/store)
set -eu

lock=${1:?usage: publish-toolchain-subst.sh LOCK NAME DB STORE_DIR OUT_STORE}
name=${2:?usage: publish-toolchain-subst.sh LOCK NAME DB STORE_DIR OUT_STORE}
db=${3:?}; storedir=${4:?}; out=${5:?}
: "${TD_BUILDER:?TD_BUILDER unset}"
: "${TD_SUBST_BIN:?TD_SUBST_BIN unset}"
: "${TD_SUBST_PRIVKEY:?TD_SUBST_PRIVKEY unset (the daily-runner private key is a host secret)}"
TD_STORE_DIR=${TD_STORE_DIR:-/td/store}; export TD_STORE_DIR

path=$("$TD_BUILDER" toolchain-path "$lock" "$name")
base=$(basename "$path")
mkdir -p "$out"
"$TD_BUILDER" subst-export "$db" "$storedir" "$out" "$path" >/dev/null \
  || { echo "publish: subst-export failed for $path" >&2; exit 1; }
test -f "$out/$base.narinfo" || { echo "publish: no narinfo written for $base" >&2; exit 1; }
"$TD_SUBST_BIN" sign "$out" "$TD_SUBST_PRIVKEY" >/dev/null \
  || { echo "publish: sign failed" >&2; exit 1; }
grep -q '^Sig: ' "$out/$base.narinfo" || { echo "publish: narinfo not signed" >&2; exit 1; }

echo "published $path -> $out/$base.narinfo (signed)"
