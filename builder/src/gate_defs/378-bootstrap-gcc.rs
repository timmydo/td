//! bootstrap-gcc — source-bootstrap BRICK 5 (gcc toolchain), THE milestone: from the 229-byte seed, the
//! tcc-built GNU Make + Binutils build GCC 2.95.3 — the first real C COMPILER in the /td/store toolchain
//! (guix's gcc-core-mesboot0). The td-built patch applies guix's vendored gcc boot patch and the
//! tcc-built make drives tcc, using binutils' as/ld/ar. config.cache float hint, LANGUAGES=c, AR=ar,
//! remove-info + install2 (libgcc/libc assembly), /bin/sh-shebang rewrite (gcc helper scripts exec
//! #!/bin/sh, absent in the sandbox). i686, static, serial. DURABLE: pinned-input (7 tarballs + 2 boot
//! patches == pins), no-guix (no gcc/guile/guix; no /gnu/store in gcc/cc1), behavioral (gcc reports
//! 2.95.3 + compiles+links+runs a C program → 42), repro (byte-identical gcc+cc1). NOT a BUILD_GATE.
//! gcc-mesboot1 (4.6.4) → gcc-mesboot (4.7.4) → glibc build on this.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-gcc",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        script: r##"
echo ">> bootstrap-gcc: from the seed, the tcc-built make + binutils build GCC 2.95.3 — a real C compiler that compiles+links+runs C, guix-free + reproducible (source-bootstrap brick 5)"
sh tests/bootstrap-gcc.sh
"##,
    }
}
