//! rust-userland-x86_64-store-native — #258 workstream B ("build world" cutover): the shipped Rust
//! userland (ripgrep, the template) built by the NATIVE x86_64 /td/store toolchain (gcc 14.3.0 +
//! binutils 2.44 + glibc 2.41) instead of the guix rust + gcc-toolchain seed. Reuses gate 416's
//! assembly (native gcc + relinked rust + glibc at /td/store) as a library, provisions ripgrep's crate
//! closure GUIX-FREE (td cargo-proxy vendor set), and builds it through `td-builder build-recipe`/
//! run_rust with the /td/store toolchain (TD_SEED_STORE + TD_EXTRA_DBS) and the native link mode
//! (TD_RUST_STORE_{INTERP,RPATH,BDIR}). The produced `rg` links the /td/store glibc, RUNS in a
//! store-ns own-root with /gnu/store ABSENT, greps a needle, and is reproducible (td-builder check).
//! DURABLE: supply-chain (crate sha == the crates.io pin), native-arch (the linker is the ELF64
//! native gcc/as/ld), no-guix (rg references no guix rust/gcc-toolchain; interp/RUNPATH = /td/store),
//! behavioral (rg RUNS + greps a needle, not the unrelated file), repro (double-build agrees).
//! HEAVY (the native gcc build is ~45 min; from-seed adds the ~98-min cross build). It warms td's own
//! recipe evaluator itself (tests/recipe-eval-tool.sh), so it does NOT drag in the full build-recipes
//! corpus — it is a HEAVY gate, not a BUILD_GATE. Reuses the crate closure the check.sh prelude warms.
//! NOTE (#258 dev gate): this is the mechanism gate; the product-command cutover folds it into
//! `td shell`, while the recipe-owned ripgrep check keeps the crate-free package assertion.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "rust-userland-x86_64-store-native",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: false,
        specs: &[],
        // Typed artifact inputs (#353): the script sources tests/rust-x86_64-
        // runtime-store-native.sh (assemble-only), whose resolve/verify paths
        // consume TD_GATE_INPUT_{COREUTILS,BASH_STATIC}.
        inputs: &[
            ArtifactInput {
                name: "coreutils",
                kind: InputKind::LockEntry { lock: "tests/td-subst.lock", stem: "coreutils" },
            },
            ArtifactInput {
                name: "bash-static",
                kind: InputKind::ClosureMember {
                    lock: "tests/hello-no-guix.lock",
                    root_stem: "bash",
                    member_stem: "bash-static",
                },
            },
        ],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> rust-userland-x86_64-store-native: build ripgrep (rg 14.1.1) with the NATIVE x86_64 /td/store toolchain (guix rust + gcc-toolchain removed), run rg in a /gnu/store-absent own-root, grep a needle, reproducible"
GUIX="$TD_GUIX" ROOT="$PWD" sh tests/rust-x86_64-userland-store-native.sh
"##,
    }
}
