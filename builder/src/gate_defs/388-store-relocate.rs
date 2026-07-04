//! store-relocate — user-pm Phase 2: relocate a DYNAMIC package's closure from guix's
//! /gnu/store to td's /td/store and run it there with NO /gnu/store (the break from guix made
//! real for dynamic binaries). `td-builder store-relocate STORE-DB ROOT DEST` copies ROOT's
//! closure into DEST and rewrites every /gnu/store reference to /td//store — the length-
//! preserving (10→10), kernel-collapsed form of /td/store, so RUNPATH/interp/.rodata/scripts are
//! all handled by one binary-safe byte substitution (no patchelf). gate_bodies::store_relocate
//! relocates hello's closure and runs hello in the store-ns (Phase 0): it greets with /gnu/store
//! ABSENT (behavioral), the relocated binary has NO /gnu/store left (structural), and it matches
//! guix's hello (removable oracle). guix is only the one-time relocation SOURCE; td-builder is
//! the guix-free stage0. Heavy (stage0 + relocate a closure + a userns) → HEAVY_GATES.

use crate::gates::{GateDef, Pool, StoreMode};

// Native (typed-Rust) gate body (#318 axis 3): the bash was ported verbatim into
// `gate_bodies::store_relocate`; `script: ""` marks it native, so the runner execs
// `td-builder gate-body store-relocate` (as the stage0) under the same memory wrapper.
pub fn gate() -> GateDef {
    GateDef {
        name: "store-relocate",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: "",
    }
}
