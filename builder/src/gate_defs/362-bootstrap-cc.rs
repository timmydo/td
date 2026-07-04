//! bootstrap-cc — source-bootstrap BRICK 1 (north star: no guix BYTES). From brick 0's seed-built
//! kaem-0, td drives the stage0-posix chain (hex1→hex2→M0→cc_amd64→M2-Planet) to a MINIMAL C
//! COMPILER + the core mescc-tools (M1 assembler, hex2 linker, kaem) — all from the 229-byte seed,
//! guix-free. The minimal source set (51 hex/C/M1 files: M2libc + M2-Planet + mescc-tools +
//! AMD64) is vendored in seed/stage0/, pinned to stage0-posix-x86 3b9c2bb. ALL-DURABLE:
//! [no-guix]    the whole chain runs with guix/Guile off env; no /gnu/store in M2-Planet;
//! [behavioral] the seed-built M2-Planet COMPILES a C program, M1+hex2 assemble+link it, and the
//! ELF RUNS returning the expected value — a real working compiler+assembler+linker;
//! [repro]      two independent chain builds produce a byte-identical M2-Planet.
//! Standalone (a few hundred-KB assemblers/compilers, ~seconds) — NOT a BUILD_GATE, never pulls
//! build-recipes. Brick 2 drives these tools over mes → tinycc; bricks 4-5 reach gcc/glibc at /td/store.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-cc",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        non_blocking: false,
        script: r##"
echo ">> bootstrap-cc: from the seed, td builds M2-Planet (a minimal C compiler) + mescc-tools; it compiles+links+RUNS a C program, guix-free + reproducible (source-bootstrap brick 1)"
sh tests/bootstrap-cc.sh
"##,
    }
}
