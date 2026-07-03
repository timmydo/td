#!/bin/sh
# ci/lower-check-drvs.sh — enumerate every derivation the full `make check`
# realises, one /gnu/store/*.drv per line on stdout.
#
# This is the foundation of the CI store image (ci/build-ci-image.sh): the
# image must contain the BUILD closure of exactly what the rung ladder does,
# or the hosted runner's offline `./check.sh` reds on a missing input. To stay
# honest it reuses the rungs' OWN lowering entry points (the tests/*-drv.scm
# scripts) — there is no second hand-maintained list of what the check builds.
# The toolchain is parsed straight off check.sh's `guix shell` line for the
# same reason.
#
# The guix-system museum tier (system images, generations, registry, place,
# rootless, no-guix, memo, offline-probe — and their negative must-fail drvs)
# was retired 2026-07-02 (human direction), so this enumeration is now just:
# the sandbox toolchain, the pinned channel instance, and the drv fixtures of
# the surviving td build-engine gates.
#
# MAINTENANCE GUARD: the LOWERING_SCRIPTS check fails loudly when tests/ gains
# a *-drv.scm / *-drvs.scm this enumeration does not run — rung authors, keep
# new lowering in a tests/*-drv(s).scm.
#
# Usage: ci/lower-check-drvs.sh   (run from the repo root; host guix must be
# the pinned channel commit — callers guard this, as check.sh does)
set -eu

GUIX="guix time-machine -C channels.scm --"
tmp=$(mktemp)
trap 'rm -f "$tmp" "$tmp.err"' EXIT

# Run one lowering entry point with an honest exit: stdout to $tmp, stderr
# surfaced on failure instead of eaten, and the exit status propagating
# instead of vanishing into a pipe (the old `repl | sed` masking is how the
# rootless TD_IMAGE_DRV gap stayed silent through the first image build).
lower() {
  "$@" > "$tmp" 2> "$tmp.err" \
    || { echo "ERROR: lowering failed: $*" >&2; tail -5 "$tmp.err" >&2; exit 1; }
}

# --- Maintenance guard: every drv-lowering script in tests/ must be run by
# the enumeration loop below.
LOWERING_SCRIPTS="tests/build-hermetic-drv.scm tests/daemon-drv.scm \
tests/td-drv-build-drv.scm tests/offline-drv.scm"
for s in tests/*-drv.scm tests/*-drvs.scm; do
  [ -e "$s" ] || continue
  case " $LOWERING_SCRIPTS " in
    *" $s "*) ;;
    *) echo "ERROR: $s is not run by ci/lower-check-drvs.sh — add it to the" >&2
       echo "  enumeration loop (and LOWERING_SCRIPTS) so the CI store image" >&2
       echo "  includes its build closure." >&2
       exit 1;;
  esac
done

# --- Sandbox toolchain: parse the package list off check.sh's `guix shell
# --search-paths` line that provisions the loop toolchain profile, so it cannot
# drift. skopeo is realized by the oci-native/rust-userland-image rungs.
tools=$(sed -n 's/^    \(make bash [a-z0-9 .+-]*\) \\$/\1/p' check.sh)
test -n "$tools" || { echo "ERROR: could not parse toolchain from check.sh" >&2; exit 1; }
# shellcheck disable=SC2086
$GUIX build -d $tools skopeo

# --- Pinned channel instance (time-machine's target — needed valid in the
# CI store so the in-loop time-machine is the same warm no-op as on a dev box).
# Each repl output goes through a file, not a pipe: a pipe into sed/grep
# would mask the repl's exit status and silently drop drvs from the image.
lower $GUIX repl -L . ci/channel-instance-drv.scm
sed -n 's/^CHANNEL_DRV=//p' "$tmp"

# --- Rungs with dedicated lowering scripts (print PREFIX=...drv lines).
for s in $LOWERING_SCRIPTS; do
  [ -e "$s" ] || { echo "ERROR: $s in LOWERING_SCRIPTS does not exist" >&2; exit 1; }
  lower $GUIX repl -L . "$s"
  sed -n 's/^[A-Z0-9_]*=\(\/gnu\/store\/.*\.drv\)$/\1/p' "$tmp"
done
