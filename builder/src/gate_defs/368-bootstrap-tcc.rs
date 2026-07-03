//! bootstrap-tcc — source-bootstrap BRICK 4 (north star: no guix BYTES). From the 229-byte seed, td
//! builds Mes + MesCC (bricks 0-3), installs them, and drives MesCC over the mes-patched TinyCC
//! source (seed/sources/tcc-*.lock) to produce `tcc` — the first REAL C compiler in the chain.
//! Built i686, as guix's tcc-boot0 does; mescc runs at the guix-default MES_ARENA (20M cells — a huge
//! arena overflows 32-bit and segfaults on tcc.c). Sources td-fetched (mes + nyacc + tcc locks).
//! DURABLE: pinned-input (3 tarballs == locks), no-guix (no gcc/guile/guix on PATH; no /gnu/store in
//! tcc), behavioral (tcc compiles+links a C program that RUNS returning 42; tcc 0.9.27, 32-bit ELF),
//! repro (byte-identical tcc). Standalone (~minutes of Mes self-host + tcc) — NOT a BUILD_GATE.
//! Brick 5 builds gcc with tcc.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-tcc",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        script: r##"
echo ">> bootstrap-tcc: from the seed, MesCC builds TinyCC (tcc) — the first real C compiler; it compiles+runs a C program returning 42, guix-free + reproducible (source-bootstrap brick 4)"
sh tests/bootstrap-tcc.sh
"##,
    }
}
