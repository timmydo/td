//! recipe-checks — the gate that runs the recipe-owned package checks.
//!
//! The package-specific assertions live on the recipes themselves; this is only
//! the runner's entry point to them. The loop body is native Rust in
//! `builder/src/gate_bodies.rs`.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "recipe-checks",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        non_blocking: true,
        script: "",
    }
}
