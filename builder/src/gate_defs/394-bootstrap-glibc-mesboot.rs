//! bootstrap-glibc-mesboot — source-bootstrap BRICK 5: the MODERN C library, GNU libc 2.16.0 (guix's
//! glibc-mesboot). After glibc-mesboot0 (2.2.5), the gcc-mesboot1 (GCC 4.6.4, C++) toolchain +
//! binutils-mesboot + gawk-mesboot build glibc 2.16.0 with nptl threads — the C library the final mesboot
//! gcc (gcc-mesboot, GCC 4.9) is built against. From the 229-byte seed: chain → gcc-mesboot1 →
//! {binutils-mesboot, gawk-mesboot} → glibc 2.16.0 (two-stage: bootstrap headers then full nptl library).
//! td builds it STATIC (guix shared); library-only (nscd program + texinfo manual dropped, empty
//! soversions.mk for install). i686, static, serial. DURABLE: pinned-input (chain + 7 boot patches +
//! …/gawk-3.1.8/glibc-2.16.0), no-guix (no /gnu/store in libc.a), behavioral (a C program AND a pthread
//! (nptl) program link statically + run → 42), repro (crt byte-identical + the libc's linked output
//! byte-identical). NOT a BUILD_GATE. gcc-mesboot (GCC 4.9, the final mesboot gcc) is next.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-glibc-mesboot",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> bootstrap-glibc-mesboot: the gcc-mesboot1 toolchain builds GNU libc 2.16.0 with nptl — a modern, threaded C library from the seed, guix-free + reproducible (source-bootstrap brick 5)"
sh tests/bootstrap-glibc-mesboot.sh
"##,
    }
}
