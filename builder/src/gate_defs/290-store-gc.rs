//! store-gc (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td computes
//! the GC-reachable CLOSURE of a td-BUILT subject — the daemon's THIRD role (GC) — TWO
//! daemon-free ways, in pure Rust, over a TD-OWNED store:
//! (1) DB walk: `td-builder store-closure DB ROOT` reads the DB with td's own SQLite
//! reader (`store_db_read`) and walks the `Refs` graph from ROOT (the GC "mark" set).
//! (2) Content scan: `td-builder store-closure-scan STORE ROOT` re-derives the SAME set by
//! NAR-scanning ROOT's bytes for store-path references, transitively (the daemon's
//! scanForReferences), with NO store DB — the closure query the loop's store-native
//! gates use to resolve a runtime closure without the guix daemon or its DB.
//! R3 (guix-retirement ladder → #261): the SUBJECT is now td-BUILT (gate_bodies::store_subject —
//! a synthetic td-built subject and its
//! closure is CONTENT-SCANNED, so this gate runs with guix OFF PATH — no `guix build`, no
//! `guix gc`. The removable guix-comparison oracle (== `guix gc -R`) is DROPPED per CLAUDE.md
//! directive 3 (called out in the PR): the two TD methods are asserted equal to EACH OTHER and
//! to the staged closure (td's own internal consistency), a STRONGER feature test over a
//! td-built artifact than equality-vs-guix. A missing Refs edge or a missed byte reference
//! would make the two disagree (verified-red). BOUNDARY: the scan of an OUTPUT root is the
//! runtime closure (`.drv`-free); the destructive SWEEP is store-gc-sweep, not here. Needs
//! td-builder + the corpus build, so it slots in the heavy pool and the build-recipes prelude.

use crate::gates::{GateDef, Pool, StoreMode};

// Native (typed-Rust) gate body (#318 axis 3): the bash was ported verbatim into
// `gate_bodies::store_gc`; `script: ""` marks it native, so the runner execs
// `td-builder gate-body store-gc` (as the stage0) under the same memory wrapper.
pub fn gate() -> GateDef {
    GateDef {
        name: "store-gc",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        inputs: &[],
        store: StoreMode::Private, // cold by design (#317 audit): GC semantics assert exact contents of a fresh fixture store
        non_blocking: true,
        script: "",
    }
}
