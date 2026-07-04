//! bootstrap-gcc-core-mesboot1 — source-bootstrap BRICK 5: GCC 4.6.4 (C) — the first MODERN, modular gcc
//! (guix's gcc-core-mesboot1). After gcc-mesboot0 (2.95.3) td jumps to GCC 4.6.4, built by gcc-mesboot0 +
//! binutils-mesboot1 + make-mesboot against glibc, with gmp 4.3.2 / mpfr 2.4.2 / mpc 1.0.3 unpacked
//! in-tree. The capable gcc the gcc-mesboot (4.7.4) / final-toolchain arc builds on. From the 229-byte
//! seed: chain → gcc-mesboot0 → binutils-mesboot1 → make-mesboot → gcc-core-mesboot1 (static; LDFLAGS=
//! -static since td's glibc is static-only; MAKEINFO=true skips texinfo docs). i686, static, serial.
//! DURABLE: pinned-input (chain + 5 boot patches + gcc-4.6.4/gmp/mpfr/mpc), no-guix (no /gnu/store in
//! gcc/cc1), behavioral (the modern gcc compiles+links+runs C → 42), repro (byte-identical gcc+cc1). NOT
//! a BUILD_GATE. gcc-mesboot1 (+c++) then gcc-mesboot (4.7.4) are next.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-gcc-core-mesboot1",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> bootstrap-gcc-core-mesboot1: the toolchain builds GCC 4.6.4 (C) with in-tree gmp/mpfr/mpc — a modern gcc from the seed, guix-free + reproducible (source-bootstrap brick 5)"
sh tests/bootstrap-gcc-core-mesboot1.sh
"##,
    }
}
