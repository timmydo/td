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
//! tb resolution: stage0-place (the lock-keyed CURRENT stage0), like build-daemon/daemon-recipe —
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
    }
}
