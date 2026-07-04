//! loop-sandbox (DESIGN §7.1; gate-2 "Loop tooling convergence"). td's OWN sandbox
//! (`td-builder host-sandbox`) is THE loop container (check.sh runs the whole loop in
//! it — there is no `guix shell -C` fallback). This gate is an INTRINSIC self-test of
//! that sandbox's hermetic surface — no `guix shell -C` oracle (the human's direction
//! 2026-06-14: td is the sole sandbox, no dependency on guix and no way to switch back).
//! It spawns a fresh `td-builder host-sandbox` and asserts:
//! (1) STORE + DAEMON SOCKET + GUIX exposed — `guix build -d hello` lowers to a valid
//! hello .drv inside it (a real daemon round-trip; drop the socket bind and this
//! errors → red). (2) ISOLATION — the host worktree ($PWD/AGENTS.md) is INVISIBLE
//! while /gnu/store + the socket remain exposed (a real container, not a bare userns).
//! (3) PID NAMESPACE + PRIVATE /proc — the command runs as PID 1 (not the host's
//! root-owned shepherd) and /proc shows only the sandbox's own PIDs. (4) NET
//! NAMESPACE — its own netns (inode differs from the ambient), loopback-only, with the
//! daemon still reachable over the Unix socket. Self-discriminating per leg; full
//! `guix shell -C` parity (PID/proc/net/mount/user ns) is what lets every gate —
//! including rootless and this one — run nested in td's sandbox. Heavy (a td-builder
//! compile + a few nested-sandbox guix/bash probes), in the heavy pool.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "loop-sandbox",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> loop-sandbox: td's OWN sandbox provides the hermetic loop surface (store ro + daemon socket + guix, host isolation, own PID + net namespaces) — intrinsic, no guix shell -C oracle"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
test -x "$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
realbash=`readlink -f "$(command -v bash)"`; \
realreadlink=`readlink -f "$(command -v readlink)"`; \
echo ">> store + daemon socket + guix exposed: 'guix build -d hello' inside td's sandbox lowers to a valid hello .drv (a real daemon round-trip)"; \
tdout=`"$tb" host-sandbox -- guix build -d hello`; \
echo "   td host-sandbox: $tdout"; \
case "$tdout" in /gnu/store/*-hello-*.drv) : ;; *) echo "FAIL: td's sandbox did not lower hello to a .drv ('$tdout') — store/socket/guix exposure is broken" >&2; exit 1;; esac; \
echo ">> isolation: the host worktree is invisible inside td's sandbox, while the store + socket stay exposed"; \
if "$tb" host-sandbox -- "$realbash" -c "test -e '$PWD/AGENTS.md'"; then \
  echo "FAIL: the host worktree ($PWD) leaked into td's sandbox — not isolated" >&2; exit 1; fi; \
"$tb" host-sandbox -- "$realbash" -c "test -d /gnu/store && test -S /var/guix/daemon-socket/socket" \
  || { echo "FAIL: td's sandbox did not expose /gnu/store + the daemon socket" >&2; exit 1; }; \
echo "   worktree gone; /gnu/store + daemon socket exposed"; \
echo ">> PID namespace + private /proc: the command is PID 1 (not the host's root-owned shepherd) and /proc shows only the sandbox's own processes"; \
pid1=`"$tb" host-sandbox -- "$realbash" -c 'read c < /proc/1/comm; printf %s "$c"'`; \
test "$pid1" = bash \
  || { echo "FAIL: PID 1 inside td's sandbox is '$pid1', expected the sandbox command (bash) — no private PID namespace" >&2; exit 1; }; \
npids=`"$tb" host-sandbox -- "$realbash" -c 'n=0; for d in /proc/[0-9]*; do n=$((n+1)); done; printf %s "$n"'`; \
test "$npids" -le 3 \
  || { echo "FAIL: $npids PIDs visible inside td's sandbox — /proc is not private to the sandbox's PID namespace" >&2; exit 1; }; \
echo "   PID 1 = the sandbox command; $npids PID(s) visible (private /proc)"; \
echo ">> net namespace: td's sandbox runs in its OWN netns, loopback-only, daemon reachable across it"; \
parent_ns=`readlink /proc/self/ns/net`; \
td_ns=`"$tb" host-sandbox -- "$realbash" -c 'exec "$0" /proc/self/ns/net' "$realreadlink"`; \
echo "   ambient netns: $parent_ns ; td host-sandbox netns: $td_ns"; \
case "$td_ns" in net:\[*\]) : ;; *) echo "FAIL: td's sandbox netns '$td_ns' is not a net namespace link" >&2; exit 1;; esac; \
test "$td_ns" != "$parent_ns" \
  || { echo "FAIL: td's sandbox did not enter its OWN netns — no net isolation" >&2; exit 1; }; \
"$tb" host-sandbox -- "$realbash" -c 'ifaces=""; while IFS= read -r l; do case "$l" in *:*) n="${l%%:*}"; ifaces="$ifaces ${n// /}";; esac; done < /proc/net/dev; test "$ifaces" = " lo"' \
  || { echo "FAIL: td's sandbox netns is not loopback-only (a non-lo interface is present)" >&2; exit 1; }; \
echo "   td entered its own loopback-only netns; the daemon stayed reachable (the lowering above held across it)"; \
echo "PASS: td's OWN sandbox (td-builder host-sandbox) provides the hermetic loop surface — store (ro) + daemon socket + guix exposed (hello lowered to its .drv), host filesystem isolated (worktree invisible), running as PID 1 of its own PID namespace with a private /proc, in its own loopback-only network namespace; intrinsic self-test, no guix shell -C oracle."
"##,
    }
}
