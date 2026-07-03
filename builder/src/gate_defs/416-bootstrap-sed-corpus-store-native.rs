//! bootstrap-sed-corpus-store-native — source-bootstrap BRICK 8 (retire the guix toolchain seed): a SECOND real
//! corpus package built by td's OWN /td/store toolchain, after GNU hello (bootstrap-hello-corpus-store-native) —
//! the same engine path applied to GNU sed. "More corpus on the /td/store toolchain": drives the guix
//! gcc-toolchain out of the corpus baseline. From the 229-byte seed td builds the chain → GCC 4.9.4 → MODERN GCC
//! 14.3.0 + binutils 2.44 → MODERN glibc 2.41 (the full /td/store toolchain), then with THAT toolchain
//! (substituted for guix's gcc-toolchain-15.2.0) `td-builder build-recipe` builds a REAL corpus package — GNU
//! sed 4.9, the exact version sed-no-guix.lock builds with guix's gcc-toolchain — chained via the engine's
//! closure_multi (TD_EXTRA_DBS) + multi-prefix sandbox staging + 32-bit ELF interp rewriting. The sed binary
//! links the /td/store glibc 2.41, references NO guix gcc-toolchain, and runs in the own-root performing a real
//! text substitution (foo→bar), /gnu/store ABSENT. DURABLE: pinned-input, no-guix, no-guix-toolchain (no guix
//! gcc-toolchain ref in the binary), behavioral (a text processor actually transforms text from /td/store),
//! structural (own-root /td/store, no /gnu/store). Builds the full toolchain from the seed (heavy, ~90min). NOT a BUILD_GATE.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-sed-corpus-store-native",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> bootstrap-sed-corpus-store-native: the /td/store MODERN toolchain (gcc 14.3.0 + binutils 2.44 + glibc 2.41, all from the seed) builds REAL corpus GNU sed 4.9 via build-recipe — substituted for guix's gcc-toolchain-15.2.0; sed links /td/store glibc 2.41, no guix gcc-toolchain ref, runs → substitutes foo→bar, /gnu/store ABSENT (source-bootstrap brick 8 — 2nd corpus package, after hello)"
sh tests/bootstrap-sed-corpus-store-native.sh
"##,
    }
}
