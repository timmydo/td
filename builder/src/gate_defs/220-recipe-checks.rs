//! recipe-checks — package behavior/repro checks live on the package recipes.
//!
//! This replaces the package-specific gate files that only proved "the recipe
//! builds and the resulting package does its basic job": corpus-no-guix,
//! toolchain-no-guix, corpus-deps-no-guix, rust-*-crate-free, rust-vendor,
//! rust-russh, rust-fetch, cmake, and the first store-native corpus fan-out
//! gates. The single gate body has no per-package case table; it asks
//! td-recipe-eval for recipe-owned check steps and runs them.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "recipe-checks",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        // PR-scoped build-recipes prebuilds the PR-tier corpus check. Daily
        // recipe checks still build/check through the same cache-lib helpers
        // when the full `check` runs them.
        specs: &["hello"],
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
bash tests/recipe-checks.sh
"##,
    }
}
