//! bootstrap-toolchain-store-native — place the seed-built STATIC mesboot toolchain at td's OWN store
//! /td/store and prove it COMPILES+RUNS from there in td's own root, /gnu/store ABSENT. The first
//! /td/store-native step after the chain reaches GCC 4.9.4: the toolchain bytes are already guix-free
//! (the bootstrap [no-guix] legs) and STATIC (no RUNPATH) → no relocation; `store-add-recursive` interns
//! gcc-mesboot + binutils-mesboot + glibc-mesboot content-addressed into /td/store, and `store-ns` runs
//! gcc-mesboot THERE to compile+link a static C program → 42 with /gnu/store absent. The registered
//! /td/store path td-subst can serve (chain-caching), and the unmixed base the userland is built on.
//! DURABLE: pinned-input, no-guix (no /gnu/store in gcc/cc1), content-addr (/td/store/<hash>-name),
//! behavioral (compiles+runs from /td/store → 42), structural (/td/store is the store, /gnu/store ABSENT).

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-toolchain-store-native",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        // Typed artifact input (#353): the runnable static-bash fixture from the
        // pinned closure — resolved by the runner; the body's grep +
        // store-closure-scan hand-wiring is deleted.
        inputs: &[ArtifactInput {
            name: "bash-static",
            kind: InputKind::ClosureMember {
                lock: "tests/hello-no-guix.lock",
                root_stem: "bash",
                member_stem: "bash-static",
            },
        }],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> bootstrap-toolchain-store-native: the seed-built static mesboot toolchain is interned at /td/store and compiles+runs a C program → 42 from td's own store, /gnu/store ABSENT (first /td/store-native step)"
sh tests/bootstrap-toolchain-store-native.sh
"##,
    }
}
