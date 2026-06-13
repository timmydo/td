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
# MAINTENANCE GUARDS: if a rung is added whose artifacts are lowered by a NEW
# entry point, this script must learn it. The KNOWN_RUNGS check below fails
# loudly when the Makefile's rung pools change (update KNOWN_RUNGS and the
# enumeration together), and the LOWERING_SCRIPTS check fails loudly when
# tests/ gains a *-drv.scm / *-drvs.scm this enumeration does not run (an
# EXISTING rung growing a new lowering script — td-builder S3 was the proving
# case); the loop runs LOWERING_SCRIPTS itself, so adding the entry IS the
# enumeration edit. Residual drift this cannot see: lowering scripts named
# outside the glob (e.g. tests/td-builder-nar.scm — offline today only
# because its fixture closure coincides with the S3 scripts') and repl
# expressions inlined in Makefile recipes — rung authors, keep new lowering
# in a tests/*-drv(s).scm.
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

# --- Maintenance guard: the rung list this enumeration was written against.
KNOWN_RUNGS="eval diff typed-coverage oci-diff manifest-diff generation-diff \
rollback generation-image no-guix manifest-check oci container rootless \
oci-load registry verify-place reset test place build boot-disk td-builder \
run offline memo"
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

# --- Maintenance guard 2: every drv-lowering script in tests/ must be run by
# the enumeration loop below (imperative-surface.scm is loop-covered too but
# named outside the glob).
LOWERING_SCRIPTS="tests/manifest-image-drv.scm tests/generation-image-drv.scm \
tests/place-drv.scm tests/rollback-drv.scm tests/imperative-surface.scm \
tests/rootless-drvs.scm tests/td-builder-drv.scm tests/td-builder-s3-drvs.scm \
tests/td-builder-s4-drv.scm tests/registry-drv.scm tests/verify-place-drv.scm \
tests/offline-drv.scm tests/check-memo-drvs.scm"
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

# --- Sandbox toolchain: parse the package list off check.sh's guix shell
# line so it cannot drift. skopeo is built by the oci-load rung itself.
tools=$(sed -n 's/^  \(make bash .*\) -- \\$/\1/p' check.sh)
test -n "$tools" || { echo "ERROR: could not parse toolchain from check.sh" >&2; exit 1; }
# shellcheck disable=SC2086
$GUIX build -d $tools skopeo signify

# --- Pinned channel instance (time-machine's target — needed valid in the
# CI store so the in-loop time-machine is the same warm no-op as on a dev box).
# Each repl output goes through a file, not a pipe: a pipe into sed/grep
# would mask the repl's exit status and silently drop drvs from the image.
lower $GUIX repl -L . ci/channel-instance-drv.scm
sed -n 's/^CHANNEL_DRV=//p' "$tmp"

# --- System images (build + oci rungs) — qcow2 and docker, via -d. The
# qcow2 drv doubles as the rootless rung's target (Makefile sets
# TD_IMAGE_DRV the same way before running tests/rootless-drvs.scm).
lower $GUIX system image -L . -t qcow2 -d system/td.scm
TD_IMAGE_DRV=$(head -n1 "$tmp")
case "$TD_IMAGE_DRV" in
  /gnu/store/*.drv) ;;
  *) echo "ERROR: qcow2 lowering printed no drv path (got: '$TD_IMAGE_DRV')" >&2
     exit 1;;
esac
printf '%s\n' "$TD_IMAGE_DRV"
export TD_IMAGE_DRV
# Bare on purpose (no lower()): set -e catches failure, stderr streams live.
$GUIX system image -L . -t docker -d system/td.scm

# --- verify-place lowers against the REGISTRY's manifest digests (same
# two-step its Makefile recipe does): build the registry, skopeo-inspect it.
lower $GUIX repl -L . tests/registry-drv.scm
reg_drv=$(sed -n 's/^DRV_REGISTRY=//p' "$tmp")
case "$reg_drv" in
  /gnu/store/*.drv) ;;
  *) echo "ERROR: registry lowering printed no drv (got: '$reg_drv')" >&2; exit 1;;
esac
reg=$($GUIX build "$reg_drv")
skopeo_bin=$($GUIX build skopeo)/bin/skopeo
TD_DIGEST_1=$("$skopeo_bin" inspect --format '{{.Digest}}' "oci:$reg/oci:gen-1")
TD_DIGEST_2=$("$skopeo_bin" inspect --format '{{.Digest}}' "oci:$reg/oci:gen-2")
case "$TD_DIGEST_1$TD_DIGEST_2" in
  *sha256:*sha256:*) ;;
  *) echo "ERROR: no manifest digests from skopeo for verify-place lowering" >&2; exit 1;;
esac
export TD_DIGEST_1 TD_DIGEST_2

# --- Rungs with dedicated lowering scripts (print PREFIX=...drv lines).
for s in $LOWERING_SCRIPTS; do
  [ -e "$s" ] || { echo "ERROR: $s in LOWERING_SCRIPTS does not exist" >&2; exit 1; }
  lower $GUIX repl -L . "$s"
  sed -n 's/^[A-Z0-9_]*=\(\/gnu\/store\/.*\.drv\)$/\1/p' "$tmp"
done

# --- Marionette system tests (same two-step lowering as the Makefile recipes).
lower $GUIX repl -L . ci/system-test-drvs.scm
grep '^/gnu/store/.*\.drv$' "$tmp"
