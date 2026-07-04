//! bootstrap-gcc-mesboot0 — source-bootstrap BRICK 5: GCC 2.95.3 REBUILT by the first gcc against glibc
//! (guix's gcc-mesboot0). After glibc-mesboot0, td rebuilds gcc so the compiler is self-hosted on glibc
//! (not the mes libc the tcc-built gcc-core-mesboot0 used) — the re-baseline gcc-mesboot1 (4.6.4) builds
//! on. From the 229-byte seed: chain → gcc-core-mesboot0 → glibc → gcc-mesboot0 (CC=<first gcc>,
//! RANLIB=true, LANGUAGES=c). i686, static, serial. DURABLE: pinned-input (9 tarballs + 4 boot patches),
//! no-guix (no gcc/guile/guix; no /gnu/store in gcc/cc1), behavioral (the glibc-based gcc compiles+links+
//! runs C → 42), repro (byte-identical gcc+cc1). NOT a BUILD_GATE. binutils-mesboot1 + make-mesboot 3.82
//! then gcc-mesboot1 (4.6.4, needs gmp/mpfr/mpc) are next.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-gcc-mesboot0",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> bootstrap-gcc-mesboot0: the first gcc rebuilds GCC 2.95.3 against glibc — a gcc self-hosted on the real C library, guix-free + reproducible (source-bootstrap brick 5)"
sh tests/bootstrap-gcc-mesboot0.sh
"##,
    }
}
