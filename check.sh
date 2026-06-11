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
# Why each piece (learned in M2, see HISTORY.md):
#   --expose=/gnu/store        : -C otherwise hides the host guix binary closure.
#   --share=$HOME/.cache/guix  : pinned channel checkout — avoids a re-fetch.
#   --share=/var/guix          : daemon socket + writable profiles/GC roots.
#   host guix first on PATH     : the host *system* guix already IS the pinned
#                                 commit, so the Makefile's `time-machine` is a
#                                 no-op that hits the warm store (no re-fetch).
#   NO --network               : on purpose. Network => substitutes => nonguix.
#   guix shell --no-substitutes --no-offload : the daemon we share
#                                 (--share=/var/guix) runs on the HOST and HAS
#                                 network — container isolation does not isolate
#                                 it, and it is configured with
#                                 substitutes.nonguix.org. So dropping --network
#                                 is NOT enough: a not-yet-warm path would still
#                                 make the daemon query/fetch substitutes (incl.
#                                 nonguix) or offload to a remote builder,
#                                 violating the local-build-only + FSDG posture.
#                                 These flags must be on the OUTER `guix shell`
#                                 itself (triage #2): exporting GUIX_BUILD_OPTIONS
#                                 *inside* the spawned shell is too late — by then
#                                 the outer `guix shell` has already resolved (and,
#                                 cold, could have fetched/offloaded) the toolchain
#                                 profile. Passing them to `guix shell` forbids
#                                 substitution/offload for the environment build
#                                 too.
#   GUIX_BUILD_OPTIONS=...      : belt-and-suspenders for the guix build/system
#                                 calls the Makefile makes INSIDE the shell (the
#                                 repl-based diff rungs set the same via
#                                 `set-build-options #:use-substitutes? #f
#                                 #:offload? #f`, since `guix repl` does not read
#                                 this variable).
#
# THE CONTRACT (narrowed — honest scope). What the above GUARANTEES by
# construction is: NO binary substitutes and NO remote build offloading — every
# realisation is a LOCAL build, and nothing is pulled from a substitute server
# (incl. nonguix), cold or warm. It does NOT guarantee a fully network-free run:
# the daemon we share (--share=/var/guix) runs on the HOST and keeps its network,
# and `--no-substitutes` does not stop a *fixed-output* derivation (a `git`/`url`
# source fetch) from reaching out on a cold path. That residual is permitted by
# the hermeticity clause (CLAUDE.md prime directive 2: "offline except declared
# fixed-output fetches"), and in practice the warm store + pinned-channel guard
# below means no such fetch fires. Making cold source fetches impossible too
# would require isolating the host daemon's network or a pre-populated source
# closure — a defense-in-depth follow-up, not a property this script asserts.
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
  echo "  The offline loop assumes they match (see HISTORY.md). Refusing to run a" >&2
  echo "  check that would silently download substitutes." >&2
  exit 1
fi

# --- Offline-isolation control: the netns probe mechanism must discriminate ---
# The `offline` rung's probes assert "only `lo` in /proc/net/dev" inside
# builders (tests/offline-drv.scm). That assertion only has teeth if the same
# mechanism reports a non-loopback interface where network IS present — and the
# only place this script can observe a network-visible netns is here, on the
# host, before entering the no-network container. A host with no non-lo
# interface could not tell an isolated netns from a working one (the probes
# would be vacuously green): fail loudly instead. Interface lines in
# /proc/net/dev are "  name: ..."; the two header lines carry no colon.
if ! sed -n 's/^ *\([^ :|]*\):.*/\1/p' /proc/net/dev | grep -qv '^lo$'; then
  echo "check.sh: FATAL: the host netns shows no non-loopback interface in" >&2
  echo "  /proc/net/dev, so the offline rung's loopback-only probes cannot" >&2
  echo "  discriminate an isolated netns from a working one on this host." >&2
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

#   make -j2 --output-sync=target : bounded-parallel loop (loop-latency). The
#                                Makefile's dependency graph keeps the cheap
#                                fail-fast rungs strictly serial and first;
#                                heavy rungs then run at most two at a time
#                                (per the DESIGN §7.3 resource note), with
#                                per-target output grouping so failures stay
#                                readable. All rungs still must pass; a red
#                                stops new rungs from spawning.
#   --expose=/sys/fs/cgroup    : the M8 `run` rung runs the shipped OCI image as a
#                                rootless crun container. crun probes the host
#                                cgroup hierarchy at startup; inside `-C` the
#                                container's /sys/fs/cgroup is plain sysfs, not
#                                cgroup2, so crun aborts ("invalid file system type
#                                on /sys/fs/cgroup"). Exposing the host's real
#                                cgroup2 mount satisfies the probe. It is a
#                                read-only host-resource exposure (like
#                                --share=/var/guix), NOT a network/substitute path,
#                                so it does not weaken the offline contract; crun is
#                                additionally run with --cgroup-manager=disabled so
#                                it never writes the hierarchy.
exec guix shell -C --pure \
  --no-substitutes --no-offload \
  --expose=/gnu/store \
  --share="$HOME/.cache/guix" \
  --share=/var/guix \
  --expose=/sys/fs/cgroup \
  make bash coreutils sed grep findutils tar gzip crun -- \
  bash -c 'export PATH="'"$hostguix_dir"':$PATH"; export GUIX_BUILD_OPTIONS="--no-substitutes --no-offload"; exec make -j2 --output-sync=target "$@"' -- "$@"
