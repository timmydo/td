//! bootstrap-x86_64-native-gcc-store-native — source-bootstrap: rung X2 of the x86_64-toolchain track,
//! after the #201 CROSS rungs. X1 produced a CROSS x86_64 gcc — itself an i686 (ELF 32-bit) binary that
//! EMITS x86_64. X2 turns that into a NATIVE x86_64 gcc: with the cross toolchain (fetched as the
//! lock-keyed signed closure, or built from the 229-byte seed) td builds NATIVE x86_64 binutils 2.44 +
//! NATIVE x86_64 GCC 14.3.0 — gcc/cc1/g++ that are themselves ELF 64-bit x86_64 (--build=--host=--target
//! =x86_64), STATIC vs the /td/store x86_64 glibc 2.41. The native gcc is interned at /td/store and RUN
//! in the store-ns own-root, where it COMPILES a C and a C++ program from source and both run -> 42,
//! /gnu/store ABSENT — the compiler doing the work is itself an x86_64 binary living in td's own store
//! (the architectural self-hosting rung; a from-source gcc-rebuilds-gcc bootstrap is not claimed here).
//! DURABLE: pinned-input, native-arch (the gcc/cc1/as/ld ARE ELF64 x86_64, not the i686 cross gcc),
//! no-guix (no /gnu/store in the native gcc/cc1/as/ld or the x86_64 libc.so.6), content-addr, behavioral
//! (the native gcc RUNS and compiles+links a C AND C++ program -> 42), structural (own-root /td/store, no
//! /gnu/store). The NATIVE gcc is ALWAYS BUILT (never fetched); only its cross-toolchain prerequisite may
//! be fetched. HEAVY (the native gcc build is ~45 min; from-seed adds the ~98-min cross build). NOT a
//! BUILD_GATE. The cross rungs + own-root verify live in tests/x86_64-cross-fns.sh; the native
//! binutils/gcc BUILD is the structured Rust recipe `td-builder toolchain-recipe x86_64-native`
//! (builder/src/toolchain_x86_64.rs) — the former build_{binutils,gcc}_x86_64_native shell drivers.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-x86_64-native-gcc-store-native",
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
                    lock: "tests/hello-no-guix.lock",
                    root_stem: "bash",
                    member_stem: "bash-static",
                },
            },
        ],
        store: StoreMode::Shared,
        non_blocking: false,
        script: r##"
echo ">> bootstrap-x86_64-native-gcc-store-native: build a NATIVE x86_64 gcc 14.3.0 + binutils 2.44 (ELF 64-bit) with the cross toolchain, intern the native gcc at /td/store, and RUN it in the store-ns own-root where it compiles a C AND C++ program from source -> both 42, /gnu/store ABSENT (x86_64-toolchain rung X2 — the native, self-hosting-arch compiler)"
sh tests/bootstrap-x86_64-native-gcc-store-native.sh
"##,
    }
}
