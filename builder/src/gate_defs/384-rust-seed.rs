//! rust-seed — RUST IN THE SEED (North-Star, human 2026-06-21): td builds its own Rust
//! BUILD ENGINE (td-builder) from a FROZEN SEED carrying the rust toolchain, no guix
//! install in the build path. The Rust analog of the seed-build gate (376, which built
//! hello/C from a seed): tools/warm-seed.sh captures + unpacks the rust toolchain closure
//! (tests/td-builder-rust.lock roots + the stage0 builder's runtime refs) ONCE into a reusable
//! content-addressed cache (the #135 warm-seed rail — no per-run re-capture), and `build-recipe`s
//! td-builder (recipe-td-builder.ts, buildSystem rust) with that seed as its store DB
//! (TD_SEED_STORE/TD_SEED_DB) — so /var/guix + the live /gnu/store toolchain are out of the
//! build's input path. Proves the seed mechanism extends to the toolchain td can't self-build
//! ("it takes rust to build rust"). Composes existing primitives (warm-seed.sh + recipe-td-builder.ts
//! #84) — no builder change. guix/Guile scrubbed from PATH; guix is only the one-time capture SOURCE + the
//! removable oracle. Durable structural/behavioral/repro legs + the removable guix-seed
//! differential. Heavy (stage0 + capture + a self-host build + a double-build check), and a
//! BUILD_GATE so it slots after the parallel build-recipes fan-out (its cargo build would
//! otherwise contend for cores).

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "rust-seed",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        // Typed artifact inputs (#353): resolved by the runner —
        // the body consumes TD_GATE_INPUT_*.
        inputs: &[
            ArtifactInput {
                name: "coreutils",
                kind: InputKind::LockEntry { lock: "tests/td-builder-rust.lock", stem: "coreutils" },
            },
        ],
        store: StoreMode::Private, // cold by design (#317 audit): the Rust engine builds from the frozen rust seed alone
        non_blocking: true,
        script: r##"
echo ">> rust-seed: td builds td-builder (its Rust engine) from a FROZEN seed that carries the rust toolchain — /var/guix + live /gnu/store toolchain out of the build path; it runs, agrees with guix's, is reproducible (RUST IN THE SEED, North-Star)"
sh tests/rust-seed.sh
"##,
    }
}
