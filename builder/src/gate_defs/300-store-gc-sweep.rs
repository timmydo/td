//! store-gc-sweep (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). The
//! DESTRUCTIVE GC SWEEP — the other half of GC, after the mark/liveness `store-closure`
//! (#39), in pure Rust, no daemon. `td-builder store-gc-sweep STORE-DIR DB ROOT` computes
//! the live set (closure of ROOT over the Refs), DELETES every registered content path NOT
//! reachable from ROOT from the td-owned STORE-DIR, and rewrites the DB to the live set
//! (ValidPaths + Refs renumbered).
//! R3 (guix-retirement ladder → #261): the SUBJECT is now td-BUILT (gate_bodies::store_subject —
//! synthetic td-built subject staged into a td-OWNED store and its closure
//! CONTENT-SCANNED, so this gate runs with guix OFF PATH — no `guix build`, no `guix gc`. The
//! removable guix-comparison oracle (survivors == `guix gc -R glibc`) is DROPPED per CLAUDE.md
//! directive 3 (called out in the PR): the expected live set is td's OWN mark phase
//! (`store-closure DB glibc` — the reachable set the sweep must keep), so the gate asserts the
//! sweep KEEPS exactly what td's own liveness walk marks and DELETES the rest. Sweeping with
//! ROOT=glibc (a PROPER subset of the subject closure), the surviving store entries AND the
//! rewritten DB hold EXACTLY that reachable set and the dead paths' files are gone. Boundary:
//! the sweep deletes ONLY from the td-owned staged STORE-DIR and rewrites only its DB — the
//! host /gnu/store is NEVER touched. Needs td-builder + the corpus build → heavy pool + the
//! build-recipes prelude.

use crate::gates::{GateDef, Pool};

// Native (typed-Rust) gate body (#318 axis 3): the bash was ported verbatim into
// `gate_bodies::store_gc_sweep`; `script: ""` marks it native, so the runner execs
// `td-builder gate-body store-gc-sweep` (as the stage0) under the same memory wrapper.
pub fn gate() -> GateDef {
    GateDef {
        name: "store-gc-sweep",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        non_blocking: false,
        script: "",
    }
}
