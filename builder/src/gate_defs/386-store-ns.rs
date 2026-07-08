//! store-ns — user-pm Phase 0: td OWNS ITS OWN ROOT with its own store at /td/store, breaking
//! from guix (human 2026-06-21). `td-builder store-ns STORE-DIR -- CMD` enters a user namespace
//! pivoted into a minimal td-owned root that binds STORE-DIR at /td/store and binds NOTHING from
//! /gnu/store or /var/guix — so inside, /td/store IS the store and the host /gnu/store + guix
//! install are ABSENT. Rootless (no daemon, no root). gate_bodies::store_ns places a static binary
//! (bash-static, from the committed substitute fixture lock) into a td-owned store and runs it inside the store-ns,
//! asserting it runs from /td/store with /gnu/store absent (unmixed from the local guix). The
//! unmixed base the /td/store package manager runs in; the dynamic toolchain is relocated to
//! /td/store in Phase 2 (static sidesteps relocation here). td-builder is the guix-free stage0.
//! Heavy (stage0 + a nested userns) → HEAVY_GATES.

use crate::gates::{GateDef, Pool, StoreMode};

// Native (typed-Rust) gate body (#318 axis 3): the bash was ported verbatim into
// `gate_bodies::store_ns`; `script: ""` marks it native, so the runner execs
// `td-builder gate-body store-ns` (as the stage0) under the same memory wrapper.
pub fn gate() -> GateDef {
    GateDef {
        name: "store-ns",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: false,
        script: "",
    }
}
