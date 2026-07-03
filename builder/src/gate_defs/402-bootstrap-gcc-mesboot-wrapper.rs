//! bootstrap-gcc-mesboot-wrapper — source-bootstrap BRICK 6 (rung 2): a gcc-mesboot-WRAPPER at /td/store,
//! the enabling primitive for building real software with the dynamic /td/store toolchain. From the seed,
//! build the chain → gcc-mesboot1 + binutils-mesboot → SHARED glibc 2.16.0, intern them into /td/store, and
//! generate a wrapper `gcc` so a PLAIN invocation (no flags — as a real configure/make calls it) produces a
//! DYNAMIC /td/store binary (interp + RUNPATH = /td/store; headers/crt/libc baked in). Proven in the store-ns
//! own-root (/gnu/store ABSENT): the plain wrapped gcc compiles a single-file AND a 2-TU program → both
//! dynamic, interp=/td/store, run → 42. This is what lets the mesboot userland + the final modern toolchain
//! build with UNMODIFIED build systems (guix's gcc-mesboot-wrapper, td-native). DURABLE: pinned-input, no-guix
//! (no /gnu/store in libc.so.6), content-addr, behavioral (plain wrapped gcc → dynamic /td/store → 42),
//! structural (/td/store is the store, /gnu/store ABSENT). NOT a BUILD_GATE.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-gcc-mesboot-wrapper",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> bootstrap-gcc-mesboot-wrapper: a gcc-mesboot-wrapper at /td/store — a PLAIN gcc invocation produces a DYNAMIC /td/store binary that runs → 42, /gnu/store ABSENT (the unmodified-build-system primitive; source-bootstrap brick 6 rung 2)"
sh tests/bootstrap-gcc-mesboot-wrapper.sh
"##,
    }
}
