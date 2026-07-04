//! bootstrap-gcc-mesboot — source-bootstrap BRICK 5, the FINAL mesboot gcc: GCC 4.9.4 (guix's gcc-mesboot).
//! After glibc-mesboot (2.16.0), the gcc-mesboot1 (GCC 4.6.4, C++) toolchain + binutils-mesboot build GCC
//! 4.9.4 against the static glibc 2.16.0 — the last gcc in the Mes bootstrap chain, capable enough to
//! (re)build the modern toolchain. From the 229-byte seed: chain → … → gcc-mesboot1 → glibc 2.16.0 → GCC
//! 4.9.4 (one tarball, gmp/mpfr/mpc in-tree, no boot patch). td builds it STATIC (guix --enable-shared via
//! the gcc-mesboot1-wrapper's dynamic linker): the static-only glibc means every compile-and-run test must
//! link static, done with link-only flags (LDFLAGS=-static -B, CC_FOR_BUILD="<gcc> -static") that keep CC
//! clean so autoconf header tests aren't polluted. i686, static, serial. DURABLE: pinned-input (chain +
//! boot patches + gcc-4.9.4), no-guix (no /gnu/store in gcc/g++/cpp/cc1), behavioral (a C program AND a
//! C++ (libstdc++) program compile+link statically + run → 42), repro (gcc/cpp drivers byte-identical +
//! `gcc -S` output deterministic). NOT a BUILD_GATE. The Mes full-source bootstrap now reaches a modern GCC.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-gcc-mesboot",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> bootstrap-gcc-mesboot: the gcc-mesboot1 toolchain builds GCC 4.9.4 against glibc 2.16.0 — the final mesboot gcc, a modern C/C++ compiler from the seed, guix-free + reproducible (source-bootstrap brick 5)"
sh tests/bootstrap-gcc-mesboot.sh
"##,
    }
}
