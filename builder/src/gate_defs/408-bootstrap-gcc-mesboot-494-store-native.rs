//! bootstrap-gcc-mesboot-494-store-native — source-bootstrap BRICK 6/7 (the FINAL modern toolchain, rung B0,
//! the bridge): gcc-mesboot GCC 4.9.4 (the final mesboot gcc, WITH C++) + a C++-capable wrapper at the
//! dynamic /td/store. The 4.6.4 /td/store wrapper builds C (binutils 2.44, rung A) but modern gcc-boot0 =
//! gcc 14.3.0 needs C++14 — this bridges /td/store to 4.9.4 (full C++11). From the 229-byte seed, td builds
//! the chain → gcc-mesboot1 + binutils-mesboot + glibc 2.16.0 (STATIC, to build 4.9.4) → GCC 4.9.4, AND a
//! SHARED glibc 2.16.0 (the wrapper's runtime glibc), interns 4.9.4 + the shared glibc content-addressed into
//! /td/store, and generates a gcc/g++ WRAPPER there. Proven in the store-ns own-root (/gnu/store ABSENT): the
//! wrapped gcc AND g++ compile a DYNAMIC C and C++ program → both interp=/td/store, run → 42. C++ at /td/store
//! — the compiler modern gcc-boot0 will use. DURABLE: pinned-input, no-guix (no /gnu/store in gcc/g++/cpp/cc1
//! NOR libc.so.6), content-addr, behavioral (plain wrapped gcc/g++ → dynamic C/C++ /td/store → 42), structural
//! (/td/store is the store, /gnu/store ABSENT). NOT a BUILD_GATE. (4.9.4's repro is guarded by #185.)

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-gcc-mesboot-494-store-native",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        script: r##"
echo ">> bootstrap-gcc-mesboot-494-store-native: GCC 4.9.4 + a C++-capable gcc/g++ wrapper at /td/store — a PLAIN invocation builds a DYNAMIC C AND C++ program that runs → 42, /gnu/store ABSENT (source-bootstrap brick 6/7, final-toolchain rung B0 / the bridge)"
sh tests/bootstrap-gcc-mesboot-494-store-native.sh
"##,
    }
}
