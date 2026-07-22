//! recipe-rs — td's package + system-spec surface declared in RUST (rust-recipe-surface
//! track; the §5 move-off-Guile goal). boa/TypeScript/tsgo are RETIRED: recipes live in
//! recipes/src/recipes/<stem>.rs (one self-registering file per recipe; build.rs
//! assembles the registry — issue #295), evaluated by td-recipe-eval.
//! This gate compiles the dependency-free `td-recipe` crate OFFLINE and runs its unit
//! tests, which assert the surface is self-consistent — every recipe emits valid
//! round-tripping JSON (catalog::tests) and a mismatch is discriminated, not vacuously
//! accepted (td-recipe-eval::tests). Correctness vs upstream is recipe-checks' job, not
//! a boa oracle (boa/the `verify`-against-boa CLI subcommand are both retired — nothing
//! left to diff against).
//!
//! Offline by construction (the cargo-test pattern): the guix-free in-process resolvers
//! (`stage0::provision_rust`/`provision_cc`) put the warm rust + cc toolchain onto PATH — NOT a `guix shell`
//! process (R1, github issue #274); the crate has NO [dependencies] so `--frozen` touches no
//! network. No guix package is built via `guix build -e (system M) PKG` either — this gate
//! adds no new guix packager surface (directive 6).
//! Heavy, not fast: needs the rust toolchain, same rationale as cargo-test.
//!
//! Native (typed-Rust) gate body (#318 axis 3, tests/* deletion): formerly
//! tests/recipe-rs.sh (deleted); its self-consistency assertions now live as `#[test]`s
//! in the `recipes` crate itself (not reimplemented here), and `gate_bodies::recipe_rs`
//! just drives cargo + a thin CLI smoke of the release binary. `script: ""` marks this
//! gate native, so the runner execs `td-builder gate-body recipe-rs`.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "recipe-rs",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        non_blocking: false,
        script: "",
    }
}
