//! bootstrap-binutils — source-bootstrap BRICK 5 (gcc toolchain). From the 229-byte seed, td builds
//! Mes + MesCC + tcc + make + patch (bricks 0-4 + make/patch rungs), then the td-built `patch` applies
//! guix's vendored boot patch (seed/patches/binutils-boot-2.20.1a.patch) and the tcc-built GNU Make
//! drives tcc over GNU Binutils 2.20.1a to produce `as` + `ld` — guix's binutils-mesboot0, the first
//! real assembler/linker in the /td/store toolchain (gcc-mesboot needs them). First RECURSIVE-make
//! build, so it exercises the make-in-sandbox fixes (SHELL var + cleared MAKEFLAGS/jobserver). i686,
//! static, serial. DURABLE: pinned-input (6 tarballs + the boot patch == pins), no-guix (no
//! gcc/guile/guix; no /gnu/store in as/ld), behavioral (as/ld report 2.20.1 + assemble+link a running
//! i386 program → 42), repro (byte-identical). NOT a BUILD_GATE. gcc-core-mesboot0 (2.95.3) is next.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-binutils",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: false,
        script: r##"
echo ">> bootstrap-binutils: the tcc-built GNU Make builds GNU Binutils 2.20.1a (as + ld) from the seed — patch-applied, guix-free + reproducible (source-bootstrap brick 5)"
sh tests/bootstrap-binutils.sh
"##,
    }
}
