//! bootstrap-tools — source-bootstrap BRICK 5 (gcc toolchain), tool rungs toward binutils. From the
//! 229-byte seed, td builds Mes + MesCC + tcc (bricks 0-4), then the seed-built tcc compiles two
//! mesboot tools: gzip 1.2.4 (guix's gzip-mesboot, scripted) and pristine TinyCC 0.9.27 (guix's
//! tcc-boot — the brick-4 0.9.26 mes-fork compiles the fuller pristine 0.9.27). Neither needs make.
//! i686, static. Sources td-fetched (seed/sources/{gzip,tcc-0.9.27}-*.lock). DURABLE: pinned-input (5
//! tarballs == locks), no-guix (no gcc/guile/guix; no /gnu/store in the tools), behavioral (gzip
//! 1.2.4 runs; tcc-0.9.27 compiles+runs C → 33), repro (each byte-identical). NOT a BUILD_GATE.
//! patch + binutils (make-driven) build on these next; then gcc-2.95 → gcc-4.7 → glibc.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-tools",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: false,
        script: r##"
echo ">> bootstrap-tools: from the seed, the tcc from brick 4 compiles mesboot tools toward binutils — gzip 1.2.4 + the fuller pristine tcc 0.9.27, guix-free + reproducible (source-bootstrap brick 5)"
sh tests/bootstrap-tools.sh
"##,
    }
}
