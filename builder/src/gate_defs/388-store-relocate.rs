//! store-relocate — user-pm Phase 2: relocate a DYNAMIC package's closure from guix's
//! /gnu/store to td's /td/store and run it there with NO /gnu/store (the break from guix made
//! real for dynamic binaries). `td-builder store-relocate STORE-DB ROOT DEST` copies ROOT's
//! closure into DEST and rewrites every /gnu/store reference to /td//store — the length-
//! preserving (10→10), kernel-collapsed form of /td/store, so RUNPATH/interp/.rodata/scripts are
//! all handled by one binary-safe byte substitution (no patchelf). tests/store-relocate.sh
//! relocates hello's closure and runs hello in the store-ns (Phase 0): it greets with /gnu/store
//! ABSENT (behavioral), the relocated binary has NO /gnu/store left (structural), and it matches
//! guix's hello (removable oracle). guix is only the one-time relocation SOURCE; td-builder is
//! the guix-free stage0. Heavy (stage0 + relocate a closure + a userns) → HEAVY_GATES.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "store-relocate",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> store-relocate: relocate a dynamic package /gnu/store -> /td/store and run it with /gnu/store ABSENT (the break from guix for dynamic binaries; user-pm Phase 2)"
sh tests/store-relocate.sh
"##,
    }
}
