//! bootstrap-x86_64-self-gcc-store-native — source-bootstrap: rung X3 of the x86_64-toolchain track,
//! SELF-HOSTING (gcc rebuilds gcc). Rung X2 (gate 422) built a NATIVE x86_64 gcc, but the compiler
//! DOING that build was still the i686 CROSS gcc (ELF 32-bit) — gate 422's own docs say "a
//! from-source gcc-rebuilds-gcc bootstrap is not claimed here". This gate claims it: with the NATIVE
//! /td/store toolchain (fetched as the lock-keyed signed closure, or built from the cross toolchain,
//! itself fetched or built from the 229-byte seed) td REBUILDS x86_64 binutils 2.44 + GCC 14.3.0 via
//! `td-builder toolchain-recipe x86_64-self` (builder/src/toolchain_x86_64.rs — the same code path as
//! the X2 build, parameterized by builder). The SELF toolchain is ALWAYS BUILT (never fetched); only
//! its native prerequisite may be fetched. DURABLE: pinned-input, builder-arch (IN-RECIPE: the
//! driving gcc must itself be ELF64 x86_64 — an i686 builder reds, so X2 can't stand in), codegen
//! (the input native gcc and the self-rebuilt gcc emit BYTE-IDENTICAL -O2 -S assembly for a fixed C
//! and C++ TU — GCC's stage2-vs-stage3 fixpoint at the text level), native-arch, no-guix (no
//! /gnu/store in the self gcc/cc1/as/ld or the x86_64 libc.so.6), content-addr (interned as
//! gcc-14.3.0-x86_64-self, name-asserted distinct from the X2 artifact), behavioral (the SELF gcc
//! RUNS in the store-ns own-root and compiles a C AND C++ program → both run → 42), structural
//! (own-root /td/store, /gnu/store ABSENT). HEAVY (~45 min self build on a native-closure fetch HIT;
//! a MISS adds the ~45-min native build, from-seed adds the ~98-min cross build). NOT a BUILD_GATE.
//! The shared fetch-or-build ladder + the X3 fns live in tests/x86_64-cross-fns.sh.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-x86_64-self-gcc-store-native",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: false,
        specs: &[],
        // Typed artifact inputs (#353): resolved by the runner — the shared
        // x86_64 resolve/verify fns consume TD_GATE_INPUT_{COREUTILS,BASH_STATIC}.
        inputs: &[
            // coreutils: the x86_64_obtain_* wrappers call the subst-lib
            // resolve fns, which consume TD_GATE_INPUT_COREUTILS (#353 review
            // find — the wrapper call path was missed in the first cut).
            ArtifactInput {
                name: "coreutils",
                kind: InputKind::LockEntry { lock: "tests/td-subst.lock", stem: "coreutils" },
            },
            ArtifactInput {
                name: "bash-static",
                kind: InputKind::ClosureMember {
                    lock: "tests/td-subst.lock",
                    root_stem: "bash",
                    member_stem: "bash-static",
                },
            },
        ],
        store: StoreMode::Shared,
        non_blocking: false,
        script: r##"
echo ">> bootstrap-x86_64-self-gcc-store-native: the NATIVE /td/store toolchain rebuilds binutils 2.44 + GCC 14.3.0 (gcc-rebuilds-gcc, rung X3) — builder is ELF64 x86_64 by assertion, the rebuilt gcc emits byte-identical -O2 -S assembly to its builder, is interned at /td/store as gcc-14.3.0-x86_64-self, and compiles+runs C/C++ in the own-root -> 42, /gnu/store ABSENT"
sh tests/bootstrap-x86_64-self-gcc-store-native.sh
"##,
    }
}
