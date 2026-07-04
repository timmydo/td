//! bootstrap-glibc-shared-store-native — source-bootstrap BRICK 6 (first rung): a from-source DYNAMIC
//! toolchain at td's OWN store /td/store. From the 229-byte seed, td builds the chain → gcc-mesboot1 +
//! binutils-mesboot, a SHARED glibc 2.16.0 (libc.so.6 + ld-linux.so.2), interns it + gcc-mesboot1 +
//! binutils content-addressed into /td/store, and in td's own root (rootless userns, /gnu/store ABSENT)
//! links a DYNAMIC C program whose interpreter + RUNPATH point at /td/store and RUNS it → 42. First time
//! /td/store is baked into a running dynamic binary, unmixed from guix — the base the userland builds on.
//! The shared glibc skips the nis subdir (guix's glibc-mesboot ships no libnsl.so — found via guix-as-oracle)
//! and relocates glibc's ld scripts to bare names. DURABLE: pinned-input, no-guix (no /gnu/store in
//! libc.so.6), content-addr (/td/store/<hash>-name), behavioral (dynamic program interp=/td/store, runs → 42),
//! structural (/td/store is the store, /gnu/store ABSENT). NOT a BUILD_GATE.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-glibc-shared-store-native",
        pools: &[Pool::Daily],
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
echo ">> bootstrap-glibc-shared-store-native: the seed toolchain builds a SHARED glibc 2.16.0 and runs a DYNAMIC program from /td/store (interp+RUNPATH = /td/store) → 42, /gnu/store ABSENT — the first dynamic /td/store toolchain (source-bootstrap brick 6)"
sh tests/bootstrap-glibc-shared-store-native.sh
"##,
    }
}
