//! recipe-checks-daily — daily-tier wrapper for recipe-owned package checks.
//!
//! The package-specific assertions still live on recipes; this wrapper exists
//! only to preserve the PR/daily partition in the gate runner and affected-checks.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "recipe-checks-daily",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: true,
        specs: &[],
        inputs: &[
            ArtifactInput {
                name: "coreutils",
                kind: InputKind::LockEntry { lock: "tests/hello-no-guix.lock", stem: "coreutils" },
            },
            ArtifactInput {
                name: "bash-static",
                kind: InputKind::ClosureMember {
                    lock: "tests/hello-no-guix.lock",
                    root_stem: "bash",
                    member_stem: "bash-static",
                },
            },
            ArtifactInput {
                name: "gcc-toolchain",
                kind: InputKind::LockEntry { lock: "tests/hello-no-guix.lock", stem: "gcc-toolchain" },
            },
        ],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
TD_RECIPE_CHECK_SCOPE=daily bash tests/recipe-checks.sh
"##,
    }
}
