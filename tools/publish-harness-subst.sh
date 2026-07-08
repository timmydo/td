#!/bin/sh
# tools/publish-harness-subst.sh — dormant producer half of shipping the /td/store harness to
# guix-less runners (#314): publish the whole harness tree as a SIGNED substitute so a runner
# with an empty .td-build-cache/harness fetches it (tools/resolve-harness.sh) instead of needing a
# local heavy build to have produced it first.
#
# The old td-builder daily caller is retired because the Guix-hosted harness producer is gone. Keep
# this script as an inert helper until the recipe-graph harness path has a current producer again.
# It exports a tree as ONE nar + a fixed-name `td-harness` narinfo (td-builder harness-subst-export)
# and signs it with the runner's PRIVATE key (whose public half is the pinned tests/td-subst.pub the
# runner verifies).
#
# Usage: tools/publish-harness-subst.sh HARNESS_DIR OUT_STORE
#   HARNESS_DIR  the persisted harness tree (.td-build-cache/harness: store/ + rel + toolchain)
#   OUT_STORE    the persistent substitute store dir to write td-harness.narinfo + nar/ into
# Env:
#   TD_BUILDER        td-builder (harness-subst-export) (REQUIRED)
#   TD_SUBST_BIN      td-subst (sign) (REQUIRED)
#   TD_SUBST_PRIVKEY  the daily-runner's ed25519 private key, pkcs8 (REQUIRED, host secret)
set -eu

hdir=${1:?usage: publish-harness-subst.sh HARNESS_DIR OUT_STORE}
out=${2:?usage: publish-harness-subst.sh HARNESS_DIR OUT_STORE}
: "${TD_BUILDER:?TD_BUILDER unset}"
: "${TD_SUBST_BIN:?TD_SUBST_BIN unset}"
: "${TD_SUBST_PRIVKEY:?TD_SUBST_PRIVKEY unset (the daily-runner private key is a host secret)}"

{ [ -d "$hdir/store" ] && [ -s "$hdir/rel" ]; } \
  || { echo "publish-harness: $hdir is not a harness tree (expected store/ + rel)" >&2; exit 1; }
mkdir -p "$out"
"$TD_BUILDER" harness-subst-export "$out" "$hdir" >/dev/null \
  || { echo "publish-harness: harness-subst-export failed" >&2; exit 1; }
test -f "$out/td-harness.narinfo" || { echo "publish-harness: no narinfo written" >&2; exit 1; }
"$TD_SUBST_BIN" sign "$out" "$TD_SUBST_PRIVKEY" >/dev/null \
  || { echo "publish-harness: sign failed" >&2; exit 1; }
grep -q '^Sig: ' "$out/td-harness.narinfo" || { echo "publish-harness: narinfo not signed" >&2; exit 1; }

echo "published $hdir -> $out/td-harness.narinfo (signed)"
