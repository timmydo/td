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
#   ./check.sh                # full loop: cheap structural gates -> build-recipes -> heavy gates
#   ./check.sh eval           # a single Makefile target inside the same sandbox
#   ./check.sh check-harness  # the guix-free /td/store harness tier (see below)
#
# Why each piece (host-sandbox --expose-cwd binds these itself, builder/src/main.rs;
# rationale learned in M2, see HISTORY.md):
#   /gnu/store (ro)            : the host store — the warm closures the loop's
#                                REMAINING guix surface resolves against (the
#                                census gates' `guix repl` lowering + the pinned
#                                seed-lock realizations; a shrink-only ratchet,
#                                tests/guix-surface-shrink.expected). This bind —
#                                and the guix pieces below — go away when that
#                                in-sandbox surface reaches zero, not before.
#   ~/.cache/guix (rw)         : pinned channel checkout — avoids a re-fetch.
#   /var/guix (rw)             : daemon socket + writable profiles/GC roots.
#   host guix first on PATH     : the host *system* guix already IS the pinned
#                                 commit, so the Makefile's `time-machine` is a
#                                 no-op that hits the warm store (no re-fetch).
#   NO network                 : on purpose. Network => substitutes => unpinned bits.
#   guix shell --no-substitutes --no-offload : the daemon we share
#                                 (/var/guix) runs on the HOST and HAS
#                                 network — container isolation does not isolate
#                                 it, and it is configured with
#                                 substitute servers. So a no-network container
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
#   GUIX_BUILD_OPTIONS=...      : belt-and-suspenders for the guix build/repl
#                                 calls the gates make INSIDE the sandbox (the
#                                 repl-based drv fixtures set the same via
#                                 `set-build-options #:use-substitutes? #f
#                                 #:offload? #f`, since `guix repl` does not read
#                                 this variable).
#
# THE CONTRACT (narrowed — honest scope). What the above GUARANTEES by
# construction is: NO binary substitutes and NO remote build offloading — every
# realisation is a LOCAL build, and nothing is pulled from a substitute server,
# cold or warm. It does NOT guarantee a fully network-free run:
# the daemon we share (/var/guix) runs on the HOST and keeps its network,
# and `--no-substitutes` does not stop a *fixed-output* derivation (a `git`/`url`
# source fetch) from reaching out on a cold path. That residual is permitted by
# the hermeticity clause (CLAUDE.md prime directive 2: "offline except declared
# fixed-output fetches"), and in practice the warm store + pinned-channel guard
# below means no such fetch fires. Making cold source fetches impossible too
# would require isolating the host daemon's network or a pre-populated source
# closure — a defense-in-depth follow-up, not a property this script asserts.
set -eu

cd "$(dirname "$0")"

# --- Guix-free harness tier (host-sandbox-stage0 inc2c) -----------------------
# `./check.sh check-harness` runs a loop tier on td's OWN /td/store harness — the
# busybox + GNU make userland built guix-byte-free (gate 420) and persisted to
# .td-build-cache/harness/ — with NO guix: the guix-free stage0 td-builder enters
# host-sandbox binding the harness at /td/store (--store-at), /gnu/store + /var/guix
# ABSENT, guix off PATH, and runs the guix-free inner loop (mk/harness.mk) there.
# This is the substrate ci/daily-full-suite.sh uses on a VM with no guix installed.
#
# Provisioning the harness needs a guix CAPTURE host (gate 420 builds + persists it,
# shipped to the VM); the CONSUME half here touches no guix. It is handled BEFORE the
# guix integrity guard / toolchain prelude below so the harness tier never invokes
# guix. (stage0 td-builder runs HOST-side as the sandbox provider — its own /td/store
# relink, so it runs INSIDE the harness on a guix-less VM, is rust-store-native rung 3.)
if [ "${1-}" = check-harness ]; then
  hdir="$PWD/.td-build-cache/harness"
  if [ ! -d "$hdir/store" ] || [ ! -s "$hdir/rel" ]; then
    echo "check.sh: FATAL: no provisioned /td/store harness at $hdir." >&2
    echo "  Provision it on a guix capture host first:  ./check.sh userland-x86_64-store-native" >&2
    echo "  (builds busybox+make at /td/store + persists them here); ship $hdir to a guix-less VM." >&2
    exit 1
  fi
  hrel=$(cat "$hdir/rel")
  hbin="/td/store/$hrel/bin"
  . tests/cache-lib.sh
  export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"
  load_stage0 || { echo "check.sh: FATAL: could not build the guix-free stage0 td-builder." >&2; exit 1; }
  echo ">> check-harness: entering td's /td/store harness via the guix-free stage0 td-builder ($TB)"
  echo "   harness: $hdir/store  set: $hrel  (guix + /gnu/store ABSENT inside)"
  # $TB runs host-side (keeps host PATH for its own setup); the inner make is named by
  # absolute path and mk/harness.mk pins the recipe PATH to the harness bin (HBIN).
  exec "$TB" host-sandbox --expose-cwd --store-from "$hdir/store" --store-at /td/store --no-daemon -- \
    "$hbin/make" -f mk/harness.mk HBIN="$hbin" SHELL="$hbin/sh" check-harness-inner
fi

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

# Heavy-gate source/crate warm prelude: ON by default, OFF for the LIGHT tiers.
# `check-fast` (the cheap structural gates) and `check-engine` (engine smoke) run NONE of
# the heavy/bootstrap/rust gates, so they must not warm — or HANG warming — those gates'
# inputs: the source-bootstrap tarballs (now 17, incl. the ~100MB linux + gcc seeds) and
# the rust corpus crate closures (~2200 crates across 8 cargo-proxy warms). That host-side
# warm has no per-fetch timeout, so a single slow/unresponsive mirror could block the fast
# tier past the CI timeout (it tipped over when #178 added the gcc-mesboot1 seeds).
heavy_warm=0
for _goal in "$@"; do
  case "$_goal" in
    check-fast|check-engine) : ;;   # light tier — owns no heavy gate, needs no heavy warm
    *) heavy_warm=1 ;;              # the `check` loop or a targeted heavy gate — warm it
  esac
done

#   make -jN --output-sync=target : bounded-parallel loop (loop-latency). The
#                                Makefile's dependency graph keeps the cheap
#                                fail-fast rungs strictly serial and first;
#                                heavy rungs then run at most two at a time
#                                (N=2, per the DESIGN §7.3 resource note) — except
#                                the VM-free `check-engine` smoke tier, which runs
#                                hot (N=$(nproc), TD_CHECK_JOBS) since it spawns no
#                                VMs (see the -j selection below). Per-target output
#                                grouping
#                                keeps failures readable. All rungs still must pass;
#                                a red stops new rungs from spawning.
#   util-linux + sqlite        : gate consumers, both resolved from the warm store
#                                (sqlite is in guix's own closure), so the offline
#                                contract is unchanged: unshare/flock for the
#                                offline/hermetic rungs' netns probes and
#                                stage0-builder's placement lock; sqlite3 as the
#                                store-register/store-backend gates' parser oracle
#                                (td writes the store DB itself; sqlite3 only
#                                verifies the bytes).
#   /sys/fs/cgroup (ro)        : the OCI gates (oci-native, rust-userland-image)
#                                run td-native images as rootless crun containers.
#                                crun probes the host cgroup hierarchy at startup;
#                                without the host's real cgroup2 mount it aborts
#                                ("invalid file system type on /sys/fs/cgroup").
#                                A read-only host-resource exposure (like
#                                /var/guix), NOT a network/substitute path, so it
#                                does not weaken the offline contract; crun is
#                                additionally run with --cgroup-manager=disabled so
#                                it never writes the hierarchy.
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
# rung runs here, including the loop self-tests. `guix shell` (no -C) still
# PROVISIONS the toolchain profile — td replaces the CONTAINER, not guix's
# profile machinery.
#
# The container PROVIDER is td's own bootstrapped stage0 td-builder — the same
# guix-free provider the `check-harness` tier and every package-build gate already
# use (tests/stage0-builder.sh: cargo-compiled from builder/ with the pinned lock
# toolchain, guix/Guile off the build PATH, self-placed via its own
# store-add-builder), now the DEFAULT loop substrate (workstream E, #294):
# `guix build -e '(@ (system td-builder) td-builder)'` no longer provisions the
# loop container anywhere. Realize the pinned stage0 toolchain seed first so a
# cold machine can compile stage0 (warm-store no-op; same idiom as gate 175).
grep ' /gnu/store/' tests/td-builder-rust.lock | sed 's/^[^ ]* //' | xargs guix build >/dev/null \
  || { echo "check.sh: FATAL: could not realize the stage0 toolchain seed (regenerate tests/td-builder-rust.lock on a channel bump)" >&2; exit 1; }
. tests/cache-lib.sh
export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"
load_stage0 || { echo "check.sh: FATAL: could not build the guix-free stage0 td-builder for the loop sandbox." >&2; exit 1; }
tb="$TB"
# The packages guix shell -C would put on PATH, provisioned as a profile (no
# container); --search-paths prints the `export PATH="…"` line we splice in. The
# leading non-`$` run is the profile bin:sbin; the trailing `${PATH:+:}$PATH` (a
# shell-eval append) is dropped — we set PATH ourselves. This `guix shell` is the
# loop substrate's last guix-provisioned piece (bash is the Makefile's recipe
# shell; see the util-linux/sqlite note above); it retires when td's own /td/store
# userland can supply these tools — today the harness carries only busybox + make
# + the C toolchain (gate 420).
toolchain=$(guix shell --no-substitutes --no-offload \
    make bash coreutils sed grep findutils tar gzip crun util-linux sqlite \
    --search-paths | sed -n 's/^export PATH="\([^$]*\).*/\1/p' | head -n1)
[ -n "$toolchain" ] || { echo "check.sh: FATAL: could not provision the loop toolchain PATH." >&2; exit 1; }
# --- Warm-prelude robustness + parallelism (loop-latency L3) -------------------
# Two problems with the host warm: (1) NO per-step timeout, so one slow/hung
# mirror can stall the prelude past the
# CI timeout (the in-file hang-risk note above); (2) the ~10 cargo-proxy warms
# ran SERIALLY though each is fully INDEPENDENT (its own OS-picked loopback port
# + work dir — "concurrent agents/worktrees never collide"). Fix: bound every
# warm with `timeout`, and fan the cargo-proxy warms out in batches of
# TD_WARM_JOBS. Tunables: TD_WARM_TIMEOUT (per-step seconds, default 600),
# TD_WARM_JOBS (parallel cargo-proxy warms, default 4 — lower it on a loaded host).
warm_timeout=${TD_WARM_TIMEOUT:-600}
warm_jobs=${TD_WARM_JOBS:-4}
if command -v timeout >/dev/null 2>&1; then
  warm() { timeout "$warm_timeout" "$@"; }
else
  warm() { "$@"; }   # no coreutils timeout: run unbounded (best-effort)
fi
# --- Substitute store warm (x64-toolchain-subst): if a prior DAILY populated a persistent signed
# substitute store (~/.td/subst: a stashed td-subst + the published lock-keyed closure), EXPOSE it by
# exporting TD_SUBST_* — host-sandbox binds ~/.td/subst ro + preserves TD_SUBST_* (builder/src/main.rs),
# so the toolchain gates FETCH the closure instead of rebuilding ~98 min from seed. No-op on a cold
# machine (echoes nothing) → the gate builds from seed (the substitute is an optimization, never a
# correctness dependency). NEVER builds td-subst here (the daily does). TD_SUBST_FORCE_BUILD=1 (set by
# ci/daily for its AUTHORITATIVE run) suppresses the exposure so the daily always builds from seed +
# republishes — otherwise a persistent store would starve the daily of its own from-seed build.
[ "${TD_SUBST_FORCE_BUILD:-0}" = 1 ] || eval "$(sh tools/warm-subst.sh)"
# --- Bootstrap-source + corpus-crate warm: td OWNS fetching the source-bootstrap tarballs ------
# AND the rust crate closures, sha256-pinned, into .td-build-cache/ for the offline heavy
# `bootstrap-*`/`rust-*` gates. The host-PREP warm ORCHESTRATION now lives in ONE structured
# `td-feed warm <action>` subcommand (feed/src/main.rs) — the former
# tools/warm-{cargo-proxy,cargo-proxy-local,bootstrap-sources,kernel-headers{,-x86_64}}.sh
# shell scripts, now typed + in-process. BEST-EFFORT (these gates are not in the fast tier):
# a runner that cannot warm them is fine, the gate enforces presence.
if [ "$heavy_warm" = 1 ]; then
  # td-fetch's own crate closure -> .td-build-cache/crate-vendor/td-fetch for `rust-fetch`
  # (a td-fetch GET, its own warm — not the cargo-proxy). BEST-EFFORT.
  warm sh tools/warm-td-fetch-crates.sh || true
  # Resolve ONE host td-feed binary (the consolidated warm runner): the gate's td-built one,
  # else a host cargo build of feed/ — done ONCE here, not raced across the parallel warms.
  tdfeed=$(ls "$PWD"/.td-build-cache/td-feed/sd/newstore/*/bin/td-feed 2>/dev/null | head -1 || true)
  if { [ -z "$tdfeed" ] || [ ! -x "$tdfeed" ]; } && command -v cargo >/dev/null 2>&1; then
    ( cd feed && cargo build --release --quiet ) && tdfeed="$PWD/feed/target/release/td-feed" || tdfeed=""
  fi
  if [ -z "$tdfeed" ] || [ ! -x "$tdfeed" ]; then
    echo "check.sh: no td-feed binary for the heavy warm (build feed/ with cargo) — skipping (best-effort; the heavy gates enforce presence)" >&2
  else
    # `td-feed warm sources` (serial-first): fetch the pinned seed/sources/*.lock bootstrap
    # tarballs + produce the i386 + x86_64 kernel UAPI headers. Daemon lifecycle stays in
    # shell — feed-ensure.sh runs the ONE shared td-feed serve daemon and we route the source
    # fetches through it (TD_FEED_BASE, egress once across worktrees), with a direct-fetch
    # fallback on a cold-feed miss.
    sources_env="TD_ROOT=$PWD"
    faddr=$(TD_FEED_BIN="$tdfeed" sh tools/feed-ensure.sh 2>/dev/null || true)
    [ -n "$faddr" ] && sources_env="$sources_env TD_FEED_BASE=http://$faddr"
    # shellcheck disable=SC2086 -- $sources_env is split into env KEY=VAL assignments on purpose
    warm env $sources_env "$tdfeed" warm sources || true
    # --- Corpus crate-guix-free warm: `td-feed warm crate` — cargo resolves+fetches each rust
    # package's WHOLE crate closure THROUGH td's OWN cargo-proxy (now bound IN-PROCESS, so no
    # background process + log scrape), the proxy verifying each .crate sha256 == the crates.io
    # index cksum (upstream pin) -> .td-build-cache/crate-vendor/<name>/ for the offline
    # `rust-<name>-crate-free` gates. Each warm is INDEPENDENT (its own loopback port + work
    # dir), so they FAN OUT in batches of $warm_jobs instead of running serially. BEST-EFFORT.
    cp_warm() { warm "$@" || echo "check.sh: cargo-proxy warm (best-effort) failed/timed out: $*" >&2; }
    _wc=0
    for _spec in \
      "ripgrep 14.1.1" \
      "sd 1.0.0" \
      "fd-find 10.2.0 fd" \
      "procs 0.14.10" \
      "eza 0.21.6" \
      "bat 0.25.0" \
      "coreutils 0.9.0 uutils" \
      "youki 0.6.0" \
      "uu_cat 0.9.0 cat"; do
      # shellcheck disable=SC2086 -- $_spec is split into the subcommand's positional args on purpose
      cp_warm env TD_ROOT="$PWD" "$tdfeed" warm crate $_spec &
      _wc=$((_wc + 1)); [ "$((_wc % warm_jobs))" -eq 0 ] && wait
    done
    # Local-source variant: the russh demo's source is the in-tree tests/russh-demo (NOT a
    # crates.io crate), so only its 188-crate DEP closure is cargo-fetched through the proxy.
    cp_warm env TD_ROOT="$PWD" "$tdfeed" warm crate-local tests/russh-demo russh &
    wait   # drain the final batch + the local warm
  fi
fi
# --- Shared build daemon: the loop's SINGLE machine-wide build limiter --------------------
# Start (or reuse) ONE shared, persistent td build daemon on the HOST before entering the
# sandbox — it must outlive this check and be shared across every worktree/agent, so it
# cannot live inside the ephemeral per-check sandbox (its PID namespace dies with the check).
# The corpus build (build-recipes / cache-lib) SUBMITS drvs to it; the daemon caps concurrent
# builds at ONE global budget (TD_BUILD_JOBS, default from cores + RAM), so N concurrent
# checks can no longer oversubscribe the box or OOM it. host-sandbox binds its socket + store
# into the sandbox and preserves TD_DAEMON_SOCKET. Only the heavy tier builds the corpus.
if [ "$heavy_warm" = 1 ]; then
  TD_DAEMON_SOCKET=$(warm sh tools/build-daemon-ensure.sh 2>/dev/null || true)
  if [ -n "$TD_DAEMON_SOCKET" ]; then
    export TD_DAEMON_SOCKET
  else
    echo "check.sh: WARNING: could not start the shared build daemon (build-daemon-ensure.sh); corpus gates will fail loudly" >&2
  fi
fi
# make -j: the heavy/VM tiers (`check`, `check-system`) are capped at 2 — the DESIGN §7.3
# two-concurrent-VMs/builds ceiling. The `check-engine` SMOKE tier runs NO VM and only
# single-threaded builds (NIX_BUILD_CORES=1), so -j2 idles most of the box; run it HOT at
# $(nproc) — matching `build-recipes`' TD_BUILD_JOBS default, and auto-scaling as engine
# gates are added (make caps at the runnable-gate count anyway). Override with TD_CHECK_JOBS
# to throttle on a loaded shared host. Every other target keeps the safe -j2 (the VM ceiling
# is load-bearing there).
case " $* " in
  *" check-engine "*) jobs=${TD_CHECK_JOBS:-$(nproc)} ;;
  *) jobs=${TD_CHECK_JOBS:-2} ;;
esac
# nice/ionice the whole loop so it YIELDS to interactive work even at high load — the
# durable half of "don't bring down my machine": the daemon's global budget bounds how many
# builds run at once, and nice/ionice keeps whatever does run from starving the shell/editor
# (and covers the toolchain/rust gates that don't route through the daemon yet). TD_NICE
# tunes it (default 10); belt-and-suspenders with the budget, not a substitute for it.
nice_wrap="nice -n ${TD_NICE:-10}"
command -v ionice >/dev/null 2>&1 && nice_wrap="$nice_wrap ionice -c2 -n7"
exec $nice_wrap env \
  PATH="$hostguix_dir:$toolchain" \
  GUIX_BUILD_OPTIONS="--no-substitutes --no-offload" \
  "$tb" host-sandbox --expose-cwd -- make -j"$jobs" --output-sync=target "$@"
