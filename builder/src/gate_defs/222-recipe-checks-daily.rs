//! recipe-checks-daily — daily-tier wrapper for recipe-owned package checks.
//!
//! The package-specific assertions still live on recipes; this wrapper exists
//! only to preserve the PR/daily partition in the gate runner and affected-checks.
//! The loop body is native Rust in `builder/src/gate_bodies.rs`.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "recipe-checks-daily",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: true,
        specs: &[],
        non_blocking: true,
        script: "",
    }
}
