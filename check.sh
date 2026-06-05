#!/bin/sh
# check.sh — the single, self-contained pass/fail command for td (DESIGN.md §1.1).
#
# `make check` is the loop, but it must run *hermetically* (DESIGN §1.4: every
# build/test enters a fresh `guix shell -C --pure` container) and *offline*
# (DESIGN §5: local-only; reaching the network pulls substitutes incl.
# nonguix.org, violating the strict-FSDG posture). Getting that right needs a
# specific incantation (store/cache/daemon-socket exposure + host-guix on PATH);
# baking it here makes "the single command" real instead of tribal knowledge.
#
# Usage:
#   ./check.sh            # full loop: eval -> build(--check) -> boot test
#   ./check.sh eval       # a single Makefile target inside the same sandbox
#
# Why each piece (learned in M2, see PLAN.md):
#   --expose=/gnu/store        : -C otherwise hides the host guix binary closure.
#   --share=$HOME/.cache/guix  : pinned channel checkout — avoids a re-fetch.
#   --share=/var/guix          : daemon socket + writable profiles/GC roots.
#   host guix first on PATH     : the host *system* guix already IS the pinned
#                                 commit, so the Makefile's `time-machine` is a
#                                 no-op that hits the warm store (fully offline).
#   NO --network               : on purpose. Network => substitutes => nonguix.
#   GUIX_BUILD_OPTIONS=--no-substitutes : the daemon we share (--share=/var/guix)
#                                 runs on the HOST and HAS network — container
#                                 isolation does not isolate it, and it is
#                                 configured with substitutes.nonguix.org. So
#                                 dropping --network is NOT enough: a not-yet-warm
#                                 path would still make the daemon query/fetch
#                                 substitutes (incl. nonguix), violating the
#                                 local-build-only + FSDG posture. This forbids
#                                 substitution for every guix build/system call,
#                                 forcing local-from-source builds (the repl-based
#                                 diff rungs set the same via `set-build-options
#                                 #:substitutes? #f`, since `guix repl` does not
#                                 read this variable). The loop is then honestly
#                                 offline, not offline-by-luck-of-a-warm-cache.
set -eu

cd "$(dirname "$0")"

# --- Integrity guard: host guix must equal the pinned channel commit ----------
# The offline/no-download property holds ONLY because the host system guix is
# the exact commit channels.scm pins: time-machine to a *different* commit would
# recompute the channel-instance derivation, miss the warm store, and try to
# download it (breaking offline + FSDG). Fail loudly rather than silently going
# online.
pinned=$(sed -n 's/.*(commit *"\([0-9a-f]\{40\}\)").*/\1/p' channels.scm | head -n1)
hostcommit=$(guix describe -f recutils 2>/dev/null | sed -n 's/^commit: *//p' | head -n1)
if [ -z "$pinned" ]; then
  echo "check.sh: FATAL: could not parse pinned commit from channels.scm" >&2
  exit 1
fi
if [ "$hostcommit" != "$pinned" ]; then
  echo "check.sh: FATAL: host guix ($hostcommit) != pinned channel ($pinned)." >&2
  echo "  The offline loop assumes they match (see PLAN.md). Refusing to run a" >&2
  echo "  check that would silently download substitutes." >&2
  exit 1
fi

hostguix_dir=$(dirname "$(readlink -f "$(command -v guix)")")

# Default to the `check` loop target — NEVER an empty arg list. An empty `make`
# would run the Makefile's default goal (`container-check`), which re-invokes
# this script and recurses into nested containers until `unshare` runs out of
# namespaces. Always name a real loop target.
if [ "$#" -eq 0 ]; then
  set -- check
fi

exec guix shell -C --pure \
  --expose=/gnu/store \
  --share="$HOME/.cache/guix" \
  --share=/var/guix \
  make bash coreutils sed grep findutils -- \
  bash -c 'export PATH="'"$hostguix_dir"':$PATH"; export GUIX_BUILD_OPTIONS="--no-substitutes"; exec make "$@"' -- "$@"
