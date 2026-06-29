#!/bin/sh
# ci/lower-fast-drvs.sh — enumerate every derivation the FAST tier (Makefile
# `check-fast`: the cheap derivation-level rungs + `ts`) realises, one
# /gnu/store/*.drv per line on stdout. The small fast-tier CI store image
# (ci/build-ci-image.sh TD_TIER=fast) ships the build closure of exactly these,
# so a hosted runner runs the unmodified offline ./check.sh check-fast in
# minutes instead of pulling+compiling the whole pinned guix and OS closure.
#
# This is the fast-tier sibling of ci/lower-check-drvs.sh (which enumerates the
# FULL ladder). It emits, reusing the rungs' own lowering entry points:
#   - the pinned channel instance (every rung's time-machine target)
#   - the check.sh sandbox toolchain (so the offline `guix shell -C` has it)
#   - the pinned tsgo tarball FOD (the `ts` rung's native compiler seed = the
#     tests/td-tsgo.lock pin path; node + td-typescript retired in #111)
#   - the SYSTEM and OCI-image derivations the cheap rungs lower
#     (ci/lower-fast-drvs.scm — mirrors typed-diff/typed-coverage/oci-diff/
#     manifest-diff/generation-diff)
#
# Usage: ci/lower-fast-drvs.sh   (run from the repo root; host guix must be the
# pinned channel commit — callers guard this, as check.sh and build-ci-image.sh
# do).
set -eu

GUIX="guix time-machine -C channels.scm --"
tmp=$(mktemp)
trap 'rm -f "$tmp" "$tmp.err"' EXIT

# Honest exit: stdout to $tmp, stderr surfaced on failure, status propagated
# (the same masking-avoidance ci/lower-check-drvs.sh uses).
lower() {
  "$@" > "$tmp" 2> "$tmp.err" \
    || { echo "ERROR: lowering failed: $*" >&2; tail -5 "$tmp.err" >&2; exit 1; }
}

# --- Sandbox toolchain: parse the package list off check.sh's `guix shell
# --search-paths` line that provisions the loop toolchain profile, so it cannot
# drift (same source ci/lower-check-drvs.sh parses). The fast tier needs no
# skopeo/signify (those are heavy-rung tools).
tools=$(sed -n 's/^    \(make bash [a-z0-9 .+-]*\) \\$/\1/p' check.sh)
test -n "$tools" || { echo "ERROR: could not parse toolchain from check.sh" >&2; exit 1; }
# shellcheck disable=SC2086
$GUIX build -d $tools

# --- Pinned channel instance (time-machine's warm no-op target).
lower $GUIX repl -L . ci/channel-instance-drv.scm
sed -n 's/^CHANNEL_DRV=//p' "$tmp"

# (The `ts`/tsgo seed is gone: the TypeScript surface was retired — recipes/specs
# are declared in Rust now (rust-recipe-surface), so check-fast carries no tsgo.)

# --- The SYSTEM and OCI-image derivations the cheap rungs lower.
lower $GUIX repl -L . ci/lower-fast-drvs.scm
grep '^/gnu/store/.*\.drv$' "$tmp"
