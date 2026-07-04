//! bootstrap-hello-userland — source-bootstrap BRICK 6 (rung 3): the FIRST real userland package built from
//! source by the dynamic /td/store toolchain. From the 229-byte seed, td builds the chain → gcc-mesboot1 +
//! binutils-mesboot → a SHARED glibc 2.16.0, interns them content-addressed into /td/store, then compiles a
//! REAL autotools package — GNU hello 2.10 — from source (an unmodified ./configure && make) with that
//! toolchain. The resulting `hello` is a DYNAMIC ELF whose interp + RUNPATH point at the /td/store glibc; it
//! is interned at /td/store and RUNS in the store-ns own-root (/gnu/store ABSENT) → "Hello, world!". First
//! from-source GNU userland program built + run from /td/store, unmixed from guix — the toolchain builds real
//! software, not just self-tests. DURABLE (no guix oracle): pinned-input, no-guix (no /gnu/store bytes in
//! libc.so.6 NOR in the built hello), content-addr, behavioral (hello built from source runs → "Hello,
//! world!"), structural (/td/store is the store, /gnu/store ABSENT). NOT a BUILD_GATE.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-hello-userland",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> bootstrap-hello-userland: the /td/store toolchain compiles GNU hello 2.10 from source (configure+make); the dynamic binary runs from /td/store → \"Hello, world!\", /gnu/store ABSENT (source-bootstrap brick 6 rung 3)"
sh tests/bootstrap-hello-userland.sh
"##,
    }
}
