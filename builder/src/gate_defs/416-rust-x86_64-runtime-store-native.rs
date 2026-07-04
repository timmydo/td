//! rust-x86_64-runtime-store-native — rust-store-native track: the /td/store RUNTIME+COMPILE leg, the
//! critical-path DESIGN arrow "retarget rust toolchain to /td/store with gcc toolchain". #218 proved the
//! upstream x86_64 Rust toolchain RUNS from /td/store (rustc -vV); this gate proves it does its JOB —
//! COMPILES. rustc links final binaries through a C toolchain, and the first x86_64 gcc that can RUN in an
//! x86_64 own-root is the NATIVE gcc of #240 (rung X2; the cross gcc is an i686 binary). So: the x86_64
//! CROSS toolchain is FETCHED as the lock-keyed signed closure (else built from the 229-byte seed —
//! directive 1), td builds the NATIVE x86_64 binutils 2.44 + gcc 14.3.0 + an x86_64 libz, RELINKS the
//! upstream Rust 1.96.0 rustc + cargo to /td/store (td's own ELF rewriter, no patchelf) WITH the rustlib
//! sysroot, interns them beside the native toolchain + x86_64 glibc, and in the store-ns own-root rustc
//! RUNS, COMPILES hello.rs via the /td/store native gcc into a DYNAMIC ELF64 x86-64 binary (interp = the
//! /td/store x86_64 ld), and that binary RUNS → "…: 42", /gnu/store ABSENT. An x86_64 Rust toolchain that
//! COMPILES with no guix process AND no guix bytes in its store.
//! DURABLE: supply-chain (sha==pin), provenance (no /gnu/store upstream), native-arch (the linker is the
//! ELF64 native gcc/as/ld), no-guix (interned rust+native-gcc+glibc /gnu/store-free), structural (interp ∈
//! /td/store; complete lib closure + rustlib sysroot), behavioral (rustc RUNS, COMPILES, output RUNS).
//! HEAVY (native gcc build ~45 min; from-seed adds the ~98-min cross build). NOT a BUILD_GATE. The cross
//! rungs live in tests/x86_64-cross-fns.sh; the native binutils/gcc BUILD is the Rust recipe `td-builder
//! toolchain-recipe x86_64-native` (builder/src/toolchain_x86_64.rs); the fetch short-circuit in tests/x86_64-subst-lib.sh.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "rust-x86_64-runtime-store-native",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        // Typed artifact inputs (#353): resolved by the runner — the shared
        // x86_64 libs consume TD_GATE_INPUT_{COREUTILS,BASH_STATIC}.
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
        non_blocking: false,
        script: r##"
echo ">> rust-x86_64-runtime-store-native: build the NATIVE x86_64 gcc/binutils (rung X2), relink the upstream x86_64 Rust 1.96.0 toolchain to /td/store WITH its rustlib sysroot, and in the store-ns own-root RUN rustc/cargo AND COMPILE hello.rs via the /td/store native gcc into a DYNAMIC ELF64 binary that RUNS → 42, /gnu/store ABSENT (the rust-store-native runtime+COMPILE leg — the DESIGN 'retarget rust to /td/store' arrow)"
sh tests/rust-x86_64-runtime-store-native.sh
"##,
    }
}
