#!/bin/sh
# tools/warm-stage0-seed.sh — HOST-PREP: realize the pinned stage0 toolchain seed
# (tests/td-builder-rust.lock) into the local /gnu/store via the host guix daemon, for
# a guix-HAVING loop runner.
#
# The loop's container-provider prelude (tests/cache-lib.sh provision_stage0) no longer
# realizes the seed with `guix build` (#311 — it resolves from a td-subst store or FAILS
# CLOSED, no guix fallback). A guix-having host (CI check-fast, the daily backstop, a
# dev box after a channel bump) has no td-subst seed store, so it warms the seed
# OUT-OF-BAND here — exactly the escape tools/resolve-seed.sh's fail-closed message names
# — so provision_stage0 finds the seed present. The LOOP itself stays guix-free; this is
# host-prep, a sibling of the other tools/warm-*.sh (not a loop step, and — like them —
# outside the guix-surface scanner's scope, which covers the loop orchestration in
# tests/ci/gate_defs, not the host warms).
#
# The seed is the guix-built pin (retired last per the north star). Most of it is
# already materialized wherever a store image was imported or prior work ran;
# gcc-toolchain is a cheap UNION guix realizes offline from its (present) components, the
# one seed path a store image typically does not pre-materialize.
#
# No-op success on a guix-LESS host: that runner uses the td-subst seed path
# (resolve-seed.sh), not this. Env: TD_LOCK (default tests/td-builder-rust.lock).
set -eu

lock=${TD_LOCK:-tests/td-builder-rust.lock}
paths=$(sed -n 's/^[^ ]* \(\/gnu\/store\/[^ ]*\)$/\1/p' "$lock" 2>/dev/null) || paths=""
[ -n "$paths" ] || { echo "warm-stage0-seed: no /gnu/store seed paths in $lock (missing/malformed lock)" >&2; exit 1; }

command -v guix >/dev/null 2>&1 \
  || { echo "warm-stage0-seed: no guix on PATH — a guix-less runner uses the td-subst seed path (resolve-seed.sh), not this host warm" >&2; exit 0; }

# --no-substitutes: offline; realize gcc-toolchain's union (and validate the rest) from
# the components already in the store, never the network.
# shellcheck disable=SC2086 -- $paths is a whitespace-separated store-path list on purpose
guix build --no-substitutes $paths >/dev/null \
  || { echo "warm-stage0-seed: could not realize the stage0 seed from $lock (warm this host's /gnu/store, or check the pinned guix daemon)" >&2; exit 1; }
echo "warm-stage0-seed: realized the stage0 toolchain seed from $lock into /gnu/store (host-prep; the loop stays guix-free)" >&2
