#!/bin/sh
# scripts/cloud-guix-warm.sh — pre-populate /gnu/store so the OFFLINE loop runs.
#
# ./check.sh runs --no-substitutes: every derivation a rung realises must already
# be in the store. On a fresh box the store is empty, so the first offline check
# would fail to find (and be forbidden to fetch) its inputs. This script runs the
# SAME loop ONCE with substitutes ALLOWED and the network up — the declared setup
# window — so the substitutable closures (qemu, kernel, glibc, …) land in the
# store and td's own derivations get built. After it, ./check.sh finds everything
# local and runs honestly offline.
#
# It deliberately MIRRORS check.sh's sandbox (same -C --pure shares, same host
# guix on PATH) but DROPS --no-substitutes/--no-offload. It does NOT edit or wrap
# check.sh: check.sh is the frozen pass/fail spine, and its offline flags are part
# of the contract (weakening them needs human sign-off, CLAUDE.md §4.3). This is a
# separate, setup-only tool; the real loop stays untouched and offline.
#
# Drift note: the `guix shell` line below is kept in sync with check.sh's by hand
# (the price of not weakening check.sh). If check.sh's shares change, mirror them.
set -eu
cd "$(dirname "$0")/.."

CURRENT="$HOME/.config/guix/current"
[ -x "$CURRENT/bin/guix" ] || { echo "cloud-guix-warm: run cloud-guix-setup.sh first (no pinned guix)" >&2; exit 1; }
export PATH="$CURRENT/bin:$PATH"
hostguix_dir=$(dirname "$(readlink -f "$(command -v guix)")")
SUBS="${TD_SUBSTITUTE_URLS:-https://bordeaux.guix.gnu.org}"

echo "cloud-guix-warm: warming the loop with substitutes from $SUBS (network up)"
# Same container/shares as check.sh, MINUS the offline flags, PLUS explicit
# substitute URLs. `make check` then both warms the store and is a green dry-run.
exec guix shell -C --pure \
  --substitute-urls="$SUBS" \
  --expose=/gnu/store \
  --share="$HOME/.cache/guix" \
  --share=/var/guix \
  --expose=/sys/fs/cgroup \
  make bash coreutils sed grep findutils tar gzip crun util-linux sqlite -- \
  bash -c 'export PATH="'"$hostguix_dir"':$PATH"; export GUIX_BUILD_OPTIONS="--substitute-urls='"$SUBS"'"; exec make -j2 --output-sync=target check'
