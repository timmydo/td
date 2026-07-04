//! bootstrap-mescc — source-bootstrap BRICK 3 (north star: no guix BYTES). From brick 2's seed-built
//! mes-m2, td runs Mes's OWN C compiler MesCC (Scheme, parsing C with nyacc) to compile Mes's libc
//! and rebuild mes as mes-mescc, and to emit libc+tcc.a (the TinyCC library). Built i686 (32-bit), as
//! guix's mes-boot does (x86_64 MesCC self-host is immature; amd64 seed tools target i686 via defs).
//! Sources td-fetched (seed/sources/{mes,nyacc}-*.lock). DURABLE: pinned-input (both tarballs ==
//! locks), no-guix (no gcc/guile/guix on the build PATH; no /gnu/store in mes-mescc), behavioral
//! (mes-mescc evaluates Scheme + libc+tcc.a is a real ar archive), repro (byte-identical mes-mescc).
//! Standalone (~minutes of MesCC) — NOT a BUILD_GATE. Brick 4 links libc+tcc.a to build TinyCC.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-mescc",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: false,
        script: r##"
echo ">> bootstrap-mescc: from the seed, Mes's own MesCC compiler self-hosts mes (mes-mescc) + emits libc+tcc.a, guix-free + reproducible (source-bootstrap brick 3)"
sh tests/bootstrap-mescc.sh
"##,
    }
}
