//! recipe-rs — td's package + system-spec surface declared in RUST (rust-recipe-surface
//! track; the §5 move-off-Guile goal). boa/TypeScript/tsgo are RETIRED: recipes live in
//! recipes/src/recipes/<stem>.rs (one self-registering file per recipe; build.rs
//! assembles the registry — issue #295), evaluated by td-recipe-eval.
//! This gate compiles the dependency-free `td-recipe` crate OFFLINE, runs its unit tests,
//! then asserts (tests/recipe-rs.sh) the surface is self-consistent — every recipe + spec
//! emits valid round-tripping JSON and `verify` discriminates a mismatch (negative
//! control). Correctness vs upstream is recipe-checks' job, not a boa oracle.
//!
//! Offline by construction (the cargo-test pattern): the guix-free tools/provision-rust.sh +
//! tools/provision-cc.sh resolve the warm rust + cc toolchain onto PATH — NOT a `guix shell`
//! process (R1, github issue #274); the crate has NO [dependencies] so `--frozen` touches no
//! network. No guix package is built via `guix build -e (system M) PKG` either — this gate
//! adds no new guix packager surface (directive 6).
//! Heavy, not fast: needs the rust toolchain, same rationale as cargo-test.

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
        script: r##"
echo ">> recipe-rs: the Rust package + spec surface (td-recipe crate) is self-consistent (rust-recipe-surface)"
set -euo pipefail; \
rustpath=`sh tools/provision-rust.sh`; \
ccpath=`sh tools/provision-cc.sh`; \
scratch="$PWD/.recipe-rs-scratch"; \
rm -rf "$scratch"; mkdir -p "$scratch/home" "$scratch/target"; \
echo ">> build + unit-test the dependency-free td-recipe crate (offline, guix-free toolchain via tools/provision-{rust,cc}.sh)"; \
PATH="$rustpath:$ccpath:$PATH" \
CARGO_HOME="$scratch/home" CARGO_TARGET_DIR="$scratch/target" \
  sh -c 'cargo test --frozen --manifest-path recipes/Cargo.toml \
     && cargo build --release --frozen --manifest-path recipes/Cargo.toml' 2>&1 | tail -20; \
bin="$scratch/target/release/td-recipe-eval"; \
test -x "$bin" || { echo "ERROR: td-recipe-eval was not built at $bin" >&2; exit 1; }; \
TD_RECIPE_EVAL="$bin" sh tests/recipe-rs.sh; \
rm -rf "$scratch"
"##,
    }
}
