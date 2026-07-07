//! recipe-rs — td's package + system-spec surface declared in RUST (rust-recipe-surface
//! track; the §5 move-off-Guile goal). boa/TypeScript/tsgo are RETIRED: recipes live in
//! recipes/src/recipes/<stem>.rs (one self-registering file per recipe; build.rs
//! assembles the registry — issue #295), evaluated by td-recipe-eval.
//! This gate compiles the dependency-free `td-recipe` crate OFFLINE, runs its unit tests,
//! then asserts the surface is self-consistent — every recipe + spec emits valid
//! round-tripping JSON and `verify` discriminates a mismatch (negative control).
//! Correctness vs upstream is recipe-checks' job, not a boa oracle.
//!
//! Offline by construction (the cargo-test pattern): the guix-free tools/provision-rust.sh +
//! tools/provision-cc.sh resolve the warm rust + cc toolchain onto PATH — NOT a `guix shell`
//! process (R1, github issue #274); the crate has NO [dependencies] so `--frozen` touches no
//! network. No guix package is built via `guix build -e (system M) PKG` either — this gate
//! adds no new guix packager surface (directive 6).
//! Heavy, not fast: needs the rust toolchain, same rationale as cargo-test.
//!
//! Native (typed-Rust) gate body (#318 axis 3, tests/* deletion): the bash (formerly
//! tests/recipe-rs.sh, now deleted) was ported verbatim into `gate_bodies::recipe_rs`;
//! `script: ""` marks it native, so the runner execs `td-builder gate-body recipe-rs`.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "recipe-rs",
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
