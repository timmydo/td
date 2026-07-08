//! recipe-checks-daily — daily-tier wrapper for recipe-owned package checks.
//!
//! The package-specific assertions still live on recipes; this wrapper exists
//! only to preserve the PR/daily partition in the gate runner and affected-checks.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "recipe-checks-daily",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: true,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
TD_RECIPE_CHECK_SCOPE=daily bash tests/recipe-checks.sh
"##,
    }
}
