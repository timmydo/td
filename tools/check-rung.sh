#!/bin/sh
# check-rung.sh — DEV ITERATION helper (NOT a gate, NOT part of the loop).
#
# Run a cached-chain bootstrap dev harness INSIDE td's loop sandbox, so sandbox-only failures
# (no `bzip2`/no `/bin/sh` on PATH, env_clear + C locale, the read-only /gnu/store) surface in
# MINUTES against the already-built chain in .td-build-cache/ — instead of a ~40-min from-the-seed
# `./check.sh bootstrap-<rung>` round-trip just to discover a one-line unpack/shebang bug.
#
# It exists because the dev harnesses run on the HOST (which has bzip2, /bin/sh, a full locale),
# so they CANNOT catch the class of bug that only bites in the sandbox — every such bug this far
# (glibc's /bin/sh + lock-name, gcc-4.9.4's .tar.bz2/no-bzip2) cost a full gate run to find.
#
# This is purely an inner-loop accelerator: the AUTHORITATIVE gate still builds the whole chain
# from the 229-byte seed with substitutes off (prime directive 1 — the loop never substitutes and
# never trusts a cache). Once a harness is green here, run the real `./check.sh bootstrap-<rung>`.
#
# Usage:  sh tools/check-rung.sh <harness.sh> [args...]
#   e.g.  sh tools/check-rung.sh .td-build-cache/sbdev1/gccmboot-harness.sh
#
# The sandbox + toolchain provisioning below is kept deliberately in sync with check.sh's (same
# stage0 td-builder container provider, same `guix shell` toolchain list — notably
# WITHOUT bzip2, so the sandbox matches the gate's exactly and a missing-bzip2 bug still reproduces).
set -eu

harness=${1:?usage: sh tools/check-rung.sh <harness.sh> [args...]}
test -f "$harness" || { echo "check-rung: no such harness: $harness" >&2; exit 1; }
shift
root=$(cd "$(dirname "$0")/.." && pwd); cd "$root"

# The container provider is the guix-free stage0 td-builder, exactly as in check.sh
# (workstream E, #294): realize the pinned stage0 toolchain seed (warm-store no-op),
# then place/reuse the stage0 under .td-build-cache/stage0.
grep ' /gnu/store/' tests/td-builder-rust.lock | sed 's/^[^ ]* //' | xargs guix build >/dev/null \
  || { echo "check-rung: FATAL: could not realize the stage0 toolchain seed (regenerate tests/td-builder-rust.lock on a channel bump)" >&2; exit 1; }
. tests/cache-lib.sh
export TD_STAGE0_BASE="$root/.td-build-cache/stage0"
load_stage0 || { echo "check-rung: FATAL: could not build the guix-free stage0 td-builder for the sandbox." >&2; exit 1; }
tb="$TB"
# Exactly check.sh's loop toolchain (no bzip2 — the sandbox must match the gate's).
toolchain=$(guix shell --no-substitutes --no-offload \
    make bash coreutils sed grep findutils tar gzip crun util-linux sqlite \
    --search-paths | sed -n 's/^export PATH="\([^$]*\).*/\1/p' | head -n1)
[ -n "$toolchain" ] || { echo "check-rung: FATAL: could not provision the loop toolchain PATH." >&2; exit 1; }

echo ">> check-rung: $harness inside td-builder host-sandbox (cached chain reused; sandbox env matches the gate)" >&2
exec env \
  PATH="$toolchain" \
  GUIX_BUILD_OPTIONS="--no-substitutes --no-offload" \
  "$tb" host-sandbox --expose-cwd -- sh "$harness" "$@"
