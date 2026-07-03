//! bootstrap-make — source-bootstrap BRICK 5 (gcc toolchain), first rung. From the 229-byte seed, td
//! builds Mes + MesCC + tcc (bricks 0-4), then drives tcc (CC=tcc) over GNU Make 3.80 to produce a
//! working `make` — tcc's first substantial real-program build + the build tool the gcc/binutils rungs
//! need. Exactly guix's make-mesboot0; i686, static (no /lib/mes-loader). Sources td-fetched
//! (seed/sources/make-*.lock). DURABLE: pinned-input (4 tarballs == locks), no-guix (no gcc/guile/guix
//! on PATH; no /gnu/store in make), behavioral (the tcc-built make is a 32-bit ELF that runs +
//! reports GNU Make 3.80), repro (byte-identical make). Standalone (~minutes of the brick 0-4 chain +
//! make) — NOT a BUILD_GATE. Brick 5 next rungs: binutils, then gcc-2.95 with tcc + make + binutils.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-make",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> bootstrap-make: from the seed, the tcc from brick 4 compiles GNU Make 3.80 — its first real-program build, guix-free + reproducible (source-bootstrap brick 5, first rung)"
sh tests/bootstrap-make.sh
"##,
    }
}
