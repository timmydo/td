//! bootstrap-gcc-14-store-native — source-bootstrap BRICK 6/7 (the FINAL modern toolchain, rung B): a MODERN
//! GCC 14.3.0 (c,c++) at the dynamic /td/store — guix's gcc-boot0/gcc-final version, td-native. From the
//! 229-byte seed, td builds the chain → gcc-mesboot1 + binutils-mesboot + glibc 2.16.0 (static AND shared) →
//! GCC 4.9.4, then with 4.9.4 builds GCC 14.3.0 against glibc 2.16.0 (gmp-6.3.0/mpfr-4.2.1/mpc-1.3.1 in-tree).
//! 14.3.0 + the shared glibc are interned content-addressed at /td/store, and a gcc/g++ WRAPPER there compiles
//! PLAINLY a DYNAMIC C AND C++ (libstdc++ <vector>) program → both interp=/td/store, run in the own-root → 42,
//! /gnu/store ABSENT. guix does 4.9.4 → gcc-boot0(14.3.0,--without-headers) → glibc-final → gcc-final; td
//! own-then-diverges (it already has glibc 2.16.0) and builds a usable gcc 14.3.0 directly in one rung. Built
//! STATIC (so gcc 14's xgcc runs in the sandbox); the wrapper links DYNAMIC vs the shared glibc 2.16.0.
//! DURABLE: pinned-input, no-guix (no /gnu/store in gcc 14's gcc/g++/cpp/cc1 NOR libc.so.6), content-addr,
//! repro (a normalized double-build is nar-identical — intrinsic byte-reproducibility, no guix oracle),
//! behavioral (plain wrapped gcc/g++ → dynamic C/C++ /td/store → 42), structural (/td/store the store, /gnu/store
//! ABSENT). A current gcc at /td/store — the toolchain that unblocks retiring the guix seed. NOT a BUILD_GATE.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-gcc-14-store-native",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> bootstrap-gcc-14-store-native: a MODERN GCC 14.3.0 (c,c++) at /td/store — built from the seed via gcc-mesboot 4.9.4; a PLAIN wrapped gcc/g++ builds a DYNAMIC C AND C++ program that runs → 42, /gnu/store ABSENT (source-bootstrap brick 6/7, final-toolchain rung B)"
sh tests/bootstrap-gcc-14-store-native.sh
"##,
    }
}
