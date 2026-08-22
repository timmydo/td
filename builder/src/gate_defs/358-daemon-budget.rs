//! daemon-budget — the shared build daemon has a bounded worker pool:
//! it realizes drvs CONCURRENTLY but never more than its shared worker budget at once, ACROSS
//! independent submitters. Memory admission is separately shared by the per-user check host.
//! Drives the REAL `td-builder daemon` subcommand over a
//! real Unix socket with budget K=2 and TD_DAEMON_TEST_SLEEP_MS (a test-only slot hold, so
//! the ceiling is observable deterministically without slow real builds), fires M=6 concurrent
//! `daemon-request` submitters, and asserts the daemon's OWN concurrency log shows the peak
//! reached EXACTLY K — it parallelized up to the budget AND never exceeded it. The requests
//! use nonexistent drvs (they ERR fast); the build OUTCOME is irrelevant — the FEATURE under
//! test is the concurrency cap, and each request still occupies a build slot for the hold.
//! 
//! Verified-red: drop the semaphore in build_daemon::serve → the log shows "(6/2 active)",
//! so the typed log check yields peak 6 != 2 and the gate reds; force it serial → peak 1 != 2. (The cap
//! logic is also covered hermetically + deterministically by the build_daemon budget unit
//! test, run in the check-engine cargo-test tier.) The six clients use the
//! daemon's explicitly test-enabled no-build PROBE grammar. That fills the
//! worker semaphore without recursively entering the check host or borrowing
//! more memory grants, which would deadlock behind the enclosing gate's grant.
//! tb resolution: load_stage0 (the lock-keyed CURRENT stage0), like build-daemon/daemon-recipe —
//! NOT `ls stage0/store/*/bin/td-builder | head -1`: a warm runner accumulates placements and
//! lexicographic-first picked a STALE binary predating the `daemon` subcommand, so the socket
//! never appeared (a latent red that stayed hidden because nothing ran the full suite, #293/#268; fresh
//! checkouts have one placement and never saw it).

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "daemon-budget",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        non_blocking: false,
        script: r##"
echo ">> daemon-budget: the shared build daemon caps concurrent workers across independent submitters"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
test -x "$tb" || { echo "FAIL: no td-builder binary for the gate" >&2; exit 1; }; \
scratch="$PWD/.daemon-budget-scratch"; rm -rf "$scratch"; mkdir -p "$scratch/d"; \
sock="$scratch/sock"; budget=2; \
TD_DAEMON_TEST_BUDGET=$budget TD_DAEMON_TEST_SLEEP_MS=400 "$tb" daemon "$sock" "$scratch/unused-store-db" "$scratch/d" > "$scratch/daemon.log" 2>&1 & dpid=$!; \
trap 'kill $dpid 2>/dev/null || true; rm -rf "$scratch"' EXIT; \
t=0; while [ ! -S "$sock" ] && [ $t -lt 50 ]; do sleep 0.2; t=$((t+1)); done; \
[ -S "$sock" ] || { echo "FAIL: daemon socket never appeared" >&2; cat "$scratch/daemon.log" >&2; exit 1; }; \
	pids=""; for i in 1 2 3 4 5 6; do "$tb" daemon-budget-probe "$sock" "$i" >/dev/null 2>&1 & pids="$pids $!"; done; \
	for p in $pids; do wait "$p" || true; done; \
	stats=`"$tb" daemon-budget-check "$scratch/daemon.log" "$budget"` || { echo "FAIL: daemon did not honor its test worker budget $budget" >&2; cat "$scratch/daemon.log" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] $stats — the cap holds across submitters"; \
"$tb" daemon-request "$sock" SHUTDOWN >/dev/null 2>&1 || true; \
echo "PASS: daemon-budget — the shared build daemon caps concurrent workers at $budget across independent submitters."
"##,
    }
}
