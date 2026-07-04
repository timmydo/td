//! bootstrap-hello-corpus-store-native — source-bootstrap BRICK 8 (retire the guix toolchain seed, step 1): the
//! CORPUS is built by the /td/store toolchain, NOT guix's gcc-toolchain-15.2.0. From the 229-byte seed td builds
//! the chain → GCC 4.9.4 → MODERN GCC 14.3.0 + binutils 2.44 → MODERN glibc 2.41 (the full /td/store toolchain),
//! then with THAT toolchain (substituted for guix's gcc-toolchain) `td-builder build-recipe` builds a REAL corpus
//! package — GNU hello 2.12.2, the exact version hello-no-guix.lock builds with guix's gcc-toolchain — chained via
//! the engine's closure_multi (TD_EXTRA_DBS) + multi-prefix sandbox staging + 32-bit ELF interp rewriting. The
//! hello binary links the /td/store glibc 2.41, references NO guix gcc-toolchain, and runs in the own-root →
//! "Hello, world!", /gnu/store ABSENT. DURABLE: pinned-input, no-guix-toolchain (no guix gcc-toolchain ref in the
//! binary), behavioral (the corpus package actually runs from /td/store), structural (own-root /td/store, no
//! /gnu/store). This is the first corpus package built by td's OWN toolchain — the substitution that retires the
//! guix toolchain seed. The ~850-line seed→gcc-14.3.0+binutils-2.44+glibc-2.41 chain lives in the SHARED library
//! tests/bootstrap-chain.sh (bootstrap_modern_toolchain), sourced here (#327 — the inline copy is deleted): warm
//! (StoreMode::Shared) it reuses the machine-wide, content-keyed, NAR-verified chain bricks instead of rebuilding
//! from the seed every run (~90min cold → minutes warm), while every corpus assertion still runs. NOT a BUILD_GATE.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-hello-corpus-store-native",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> bootstrap-hello-corpus-store-native: the /td/store MODERN toolchain (gcc 14.3.0 + binutils 2.44 + glibc 2.41, all from the seed) builds REAL corpus GNU hello 2.12.2 via build-recipe — substituted for guix's gcc-toolchain-15.2.0; hello links /td/store glibc 2.41, no guix gcc-toolchain ref, runs → \"Hello, world!\", /gnu/store ABSENT (source-bootstrap brick 8 — retire the guix toolchain seed, step 1)"
sh tests/bootstrap-hello-corpus-store-native.sh
"##,
    }
}
