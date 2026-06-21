#!/bin/sh
# check.sh — the single, self-contained pass/fail command for td (DESIGN.md §1.1).
#
# `make check` is the loop, but it must run *hermetically* (DESIGN §1.4: every
# build/test enters a fresh container — td's OWN `td-builder host-sandbox`, the sole
# loop container; no `guix shell -C` fallback, no toggle) and *offline*
# (DESIGN §5: local-only; reaching the network pulls substitutes — unpinned
# binaries the loop's results would silently depend on). Getting that right needs a
# specific incantation (store/cache/daemon-socket exposure + host-guix on PATH);
# baking it here makes "the single command" real instead of tribal knowledge.
#
# Usage:
#   ./check.sh            # full loop: eval -> build(--check) -> boot test
#   ./check.sh eval       # a single Makefile target inside the same sandbox
#   TD_CHECK_FULL=1 ./check.sh   # force-full: bypass all memoized --check
#                                # verdicts (plan/check-memo.md constraint 4 —
#                                # REQUIRED for oracle re-baselines and any
#                                # suspected nondeterminism)
#
# Why each piece (learned in M2, see HISTORY.md):
#   --expose=/gnu/store        : -C otherwise hides the host guix binary closure.
#   --share=$HOME/.cache/guix  : pinned channel checkout — avoids a re-fetch.
#   --share=/var/guix          : daemon socket + writable profiles/GC roots.
#   host guix first on PATH     : the host *system* guix already IS the pinned
#                                 commit, so the Makefile's `time-machine` is a
#                                 no-op that hits the warm store (no re-fetch).
#   NO --network               : on purpose. Network => substitutes => unpinned bits.
#   guix shell --no-substitutes --no-offload : the daemon we share
#                                 (--share=/var/guix) runs on the HOST and HAS
#                                 network — container isolation does not isolate
#                                 it, and it is configured with
#                                 substitute servers. So dropping --network
#                                 is NOT enough: a not-yet-warm path would still
#                                 make the daemon query/fetch substitutes or
#                                 offload to a remote builder, violating the
#                                 local-build-only posture.
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
# realisation is a LOCAL build, and nothing is pulled from a substitute server,
# cold or warm. It does NOT guarantee a fully network-free run:
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
# download it (breaking the offline posture). Fail loudly rather than silently going
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

# --- check-memo: environment identity + force-full knob (plan/check-memo.md) --
# The --check verdict-memoization helper (tests/check-memo.sh) may green a
# reproducibility leg only on a verdict recorded in the SAME environment
# (constraint 2). That identity is computed HERE, on the host — the -C
# container cannot see /etc/machine-id — and carried in via --preserve below:
#   machine-id : this host (a verdict never travels between machines)
#   store fs type : the filesystem under /gnu/store (the 2026-06-12
#     readdir-order divergence was btrfs-vs-ext4 — environment-dependence the
#     same-environment keying must preserve detection of)
#   pinned commit : the channel pin (a bump re-keys every verdict)
# FAIL CLOSED: if any component is unknown the identity stays EMPTY and the
# helper never hits and never records — every leg runs the full --check.
# CI GATE (constraint 2: CI verdict reuse is OFF until gate 2 re-opens): under
# CI the identity is FORCED empty, so a persistent runner workspace can never
# accumulate verdicts that would loosen a required check.
# FORCE-FULL (constraint 4): TD_CHECK_FULL=1 ./check.sh bypasses all verdicts;
# oracle re-baselines (any DIGESTS.md change) and suspected nondeterminism
# MUST use it.
if [ -n "${CI-}" ] || [ -n "${GITHUB_ACTIONS-}" ]; then
  TD_CHECK_ENV=""
else
  machineid=$(cat /etc/machine-id 2>/dev/null || true)
  storefs=$(stat -f -c %T /gnu/store 2>/dev/null || true)
  if [ -n "$machineid" ] && [ -n "$storefs" ]; then
    TD_CHECK_ENV="$machineid:$storefs:$pinned"
  else
    TD_CHECK_ENV=""
  fi
fi
export TD_CHECK_ENV

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
#   util-linux + sqlite        : the `rootless` rung needs unshare/mount (the
#                                nested userns + staged-store binds) and
#                                sqlite3 (a CONSISTENT snapshot of the host
#                                store DB via sqlite's backup API — a plain cp
#                                races against the live daemon's writes). Both
#                                resolve from the warm store (sqlite is in
#                                guix's own closure), so the offline contract
#                                is unchanged.
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
#   --preserve='^TD_CHECK_'    : the check-memo identity/knobs computed above
#                                (TD_CHECK_ENV, TD_CHECK_FULL, ...) — --pure
#                                would otherwise strip them; the `memo` rung
#                                asserts TD_CHECK_ENV arrives.
# --- The hermetic container ---------------------------------------------------
# td's OWN sandbox (`td-builder host-sandbox --expose-cwd`) is THE loop container —
# the north star's one Rust sandbox stack spanning build AND run, made literal.
# There is NO `guix shell -C` fallback and NO toggle (human direction 2026-06-14:
# td is the sole sandbox; no dependency on guix's container, no way to switch
# back). td's sandbox provides the full hermetic surface — the WHOLE /gnu/store
# (ro) + the daemon socket /var/guix + a private /proc + /dev + /sys/fs/cgroup +
# the worktree + the guix cache, host guix + the toolchain on PATH, running as
# PID 1 of its own PID namespace in its own loopback-only network namespace (full
# guix-shell-C parity, asserted by the loop-sandbox/loop-rung self-tests). EVERY
# rung runs here, including `rootless` (its nested unprivileged userns builder now
# nests cleanly thanks to the PID-ns parity) and the loop self-tests. `guix shell`
# (no -C) still PROVISIONS the toolchain profile — td replaces the CONTAINER, not
# guix's profile machinery. See plan/loop-sandbox.md.
tb=$(guix build -L . -e '(@ (system td-builder) td-builder)')/bin/td-builder
[ -x "$tb" ] || { echo "check.sh: FATAL: could not build td-builder for the loop sandbox." >&2; exit 1; }
# The packages guix shell -C would put on PATH, provisioned as a profile (no
# container); --search-paths prints the `export PATH="…"` line we splice in. The
# leading non-`$` run is the profile bin:sbin; the trailing `${PATH:+:}$PATH` (a
# shell-eval append) is dropped — we set PATH ourselves.
toolchain=$(guix shell --no-substitutes --no-offload \
    make bash coreutils sed grep findutils tar gzip crun util-linux sqlite \
    --search-paths | sed -n 's/^export PATH="\([^$]*\).*/\1/p' | head -n1)
[ -n "$toolchain" ] || { echo "check.sh: FATAL: could not provision the loop toolchain PATH." >&2; exit 1; }
# GUIX_ENVIRONMENT is the profile root (what `guix shell -C` used to export) — the
# `rootless` rung binds it into its staged store. The first PATH entry is the
# profile's bin; its parent is the profile root.
guix_env=$(dirname "${toolchain%%:*}")
# --- Seed warm: td OWNS fetching the tsgo tarball (move-off-Guile §5) -----------
# The offline loop's gates read the pinned tsgo tarball (tests/td-tsgo.lock) instead of
# `guix build -e '(@ (system td-ts) td-tsgo-tarball)'`. The blob is fetched by td's OWN
# fetcher (td-fetch) here on the HOST (network), exactly where the daemon also fetches
# fixed-output seeds — the in-sandbox loop never egresses. Idempotent + near-instant once
# the store path is warm; only a cold machine pays the one-time fetch (+ td-fetch build).
sh tools/warm-tsgo.sh || { echo "check.sh: FATAL: could not warm the tsgo tarball (tools/warm-tsgo.sh)." >&2; exit 1; }
exec env \
  PATH="$hostguix_dir:$toolchain" \
  GUIX_BUILD_OPTIONS="--no-substitutes --no-offload" \
  GUIX_ENVIRONMENT="$guix_env" \
  "$tb" host-sandbox --expose-cwd -- make -j2 --output-sync=target "$@"
