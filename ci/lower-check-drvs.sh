#!/bin/sh
# ci/lower-check-drvs.sh — enumerate every derivation the full `make check`
# realises, one /gnu/store/*.drv per line on stdout.
#
# This is the foundation of the CI store image (ci/build-ci-image.sh): the
# image must contain the BUILD closure of exactly what the rung ladder does,
# or the hosted runner's offline `./check.sh` reds on a missing input. To stay
# honest it reuses the rungs' OWN lowering entry points (the tests/*-drv.scm
# scripts and the same repl expressions the Makefile recipes embed) — there is
# no second hand-maintained list of what the check builds. The toolchain is
# parsed straight off check.sh's `guix shell` line for the same reason.
#
# MAINTENANCE GUARD: if a rung is added whose artifacts are lowered by a NEW
# entry point, this script must learn it. The KNOWN_RUNGS check below fails
# loudly when the Makefile's rung pools change, so the image cannot silently
# go stale — update KNOWN_RUNGS *and* the enumeration together.
#
# Usage: ci/lower-check-drvs.sh   (run from the repo root; host guix must be
# the pinned channel commit — callers guard this, as check.sh does)
set -eu

GUIX="guix time-machine -C channels.scm --"

# --- Maintenance guard: the rung list this enumeration was written against.
KNOWN_RUNGS="eval diff typed-coverage oci-diff manifest-diff generation-diff \
rollback generation-image no-guix manifest-check oci container rootless \
oci-load reset test place build boot-disk td-builder run offline"
current=$(sed -n 's/^CHEAP_RUNGS := //p; s/^HEAVY_RUNGS := //p' Makefile | tr '\n' ' ')
for r in $current; do
  case " $KNOWN_RUNGS " in
    *" $r "*) ;;
    *) echo "ERROR: rung '$r' is not covered by ci/lower-check-drvs.sh —" >&2
       echo "  add its lowering entry point here (and to KNOWN_RUNGS) so the" >&2
       echo "  CI store image includes its build closure." >&2
       exit 1;;
  esac
done

# --- Sandbox toolchain: parse the package list off check.sh's guix shell
# line so it cannot drift. skopeo is built by the oci-load rung itself.
tools=$(sed -n 's/^  \(make bash .*\) -- \\$/\1/p' check.sh)
test -n "$tools" || { echo "ERROR: could not parse toolchain from check.sh" >&2; exit 1; }
# shellcheck disable=SC2086
$GUIX build -d $tools skopeo

# --- Pinned channel instance (time-machine's target — needed valid in the
# CI store so the in-loop time-machine is the same warm no-op as on a dev box).
$GUIX repl -L . ci/channel-instance-drv.scm 2>/dev/null \
  | sed -n 's/^CHANNEL_DRV=//p'

# --- System images (build + oci rungs) — qcow2 and docker, via -d.
$GUIX system image -L . -t qcow2  -d system/td.scm
$GUIX system image -L . -t docker -d system/td.scm

# --- Rungs with dedicated lowering scripts (print PREFIX=...drv lines).
for s in tests/manifest-image-drv.scm tests/generation-image-drv.scm \
         tests/place-drv.scm tests/rollback-drv.scm tests/imperative-surface.scm \
         tests/rootless-drvs.scm tests/td-builder-drv.scm tests/offline-drv.scm; do
  $GUIX repl -L . "$s" 2>/dev/null \
    | sed -n 's/^[A-Z0-9_]*=\(\/gnu\/store\/.*\.drv\)$/\1/p'
done

# --- Marionette system tests (same two-step lowering as the Makefile recipes).
$GUIX repl -L . ci/system-test-drvs.scm 2>/dev/null \
  | grep '^/gnu/store/.*\.drv$'
