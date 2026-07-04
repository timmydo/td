//! bootstrap-glibc — source-bootstrap BRICK 5 (gcc toolchain): the seed gcc 2.95.3 + binutils build GNU
//! C Library 2.2.5 (guix's glibc-mesboot0) — the first C LIBRARY in the /td/store toolchain. From the
//! 229-byte seed, the complete lower toolchain (mes→tcc→make→patch→binutils→gcc→glibc) now exists.
//! The td-built patch applies guix's 2 glibc boot patches; glibc builds against Linux 4.14.67 kernel
//! headers produced FROM the pinned source on the host (td-feed warm kernel-headers i386 — the sandbox can't
//! run the kernel build). i686, static, serial. DURABLE: pinned-input (9 tarballs + 4 boot patches ==
//! pins), no-guix (no gcc/guile/guix; no /gnu/store in libc.a/crt), behavioral (a C program statically
//! linked against the new glibc runs → 42), repro (byte-identical libc.a). NOT a BUILD_GATE.
//! gcc-mesboot1 (4.6.4) links against this glibc and is next.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-glibc",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> bootstrap-glibc: the seed gcc + binutils build glibc 2.2.5 (guix's glibc-mesboot0) — a real C library a program links against + runs, guix-free + reproducible (source-bootstrap brick 5)"
sh tests/bootstrap-glibc.sh
"##,
    }
}
