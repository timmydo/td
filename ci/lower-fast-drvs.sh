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
# (The guix-system museum tier and the cheap differential rungs — eval, the
# guix-dependence census, the static guix-surface census — were retired as
# guix-oracle gates, so the cheap serial-first tier is now empty and no
# SYSTEM/OCI-image or cheap-rung lowering remains in the fast tier beyond the
# pinned channel instance + the sandbox toolchain enumerated below.)
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
tools=$(cat tools/loop-toolchain.txt)
test -n "$tools" || { echo "ERROR: empty toolchain list — tools/loop-toolchain.txt missing or empty" >&2; exit 1; }
# shellcheck disable=SC2086
$GUIX build -d $tools

# --- Pinned channel instance (time-machine's warm no-op target).
lower $GUIX repl -L . ci/channel-instance-drv.scm
sed -n 's/^CHANNEL_DRV=//p' "$tmp"

# --- Stage0 toolchain seed (workstream E, #294): check.sh's loop container is
# provisioned by the guix-free stage0 td-builder on EVERY tier — cargo-compiled
# with the pinned lock toolchain (tests/td-builder-rust.lock) — so the fast
# image must carry the seed's runtime closure or the hosted runner cannot stand
# the loop container up. These are OUTPUT paths, not .drv paths: build-ci-image's
# closure walker (td-builder store-closure) and `guix archive --export -r` are
# both root-type-agnostic, so output roots export their runtime closure. Fail
# loudly if a seed path is not live on this build host (stale lock / cold store)
# rather than shipping an image the runner cannot compile stage0 from. (No
# while-in-pipeline: its subshell would swallow the exit under POSIX sh.)
for p in $(sed -n 's/^[^ ]* \(\/gnu\/store\/[^ ]*\)$/\1/p' tests/td-builder-rust.lock); do
  test -e "$p" || { echo "ERROR: stage0 seed path not in this host's store: $p (realize tests/td-builder-rust.lock first)" >&2; exit 1; }
  printf '%s\n' "$p"
done

# (The `ts`/tsgo seed is gone: the TypeScript surface was retired — recipes/specs
# are declared in Rust now (rust-recipe-surface), so check-fast carries no tsgo.
# The SYSTEM/OCI-image lowerings are gone with the museum tier, 2026-07-02.)
