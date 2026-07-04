//! bootstrap-binutils-mesboot1 — source-bootstrap BRICK 5: GNU Binutils 2.20.1a REBUILT by gcc-mesboot0
//! against glibc (guix's binutils-mesboot1). binutils-mesboot0 was tcc-built against the mes libc; now
//! that td has a real gcc + glibc it rebuilds binutils so as/ld/ar are gcc-built and glibc-linked — the
//! assembler/linker the next compiler (gcc-mesboot1, 4.6.4) is built with. From the 229-byte seed: chain
//! → gcc-mesboot0 → binutils-mesboot1 (plain configure: CC=<gcc-mesboot0>, real ar/ranlib, glibc libc).
//! i686, static, serial. DURABLE: pinned-input (9 tarballs + 4 boot patches), no-guix (no gcc/guile/guix;
//! no /gnu/store in as/ld/ar), behavioral (the new as+ld assemble+link+run C → 42), repro (byte-identical
//! as+ld). NOT a BUILD_GATE. make-mesboot 3.82 then gcc-mesboot1 (4.6.4, needs gmp/mpfr/mpc) are next.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-binutils-mesboot1",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> bootstrap-binutils-mesboot1: gcc-mesboot0 rebuilds GNU Binutils 2.20.1a against glibc — a gcc-built, glibc-linked as+ld, guix-free + reproducible (source-bootstrap brick 5)"
sh tests/bootstrap-binutils-mesboot1.sh
"##,
    }
}
