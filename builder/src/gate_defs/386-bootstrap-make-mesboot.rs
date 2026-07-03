//! bootstrap-make-mesboot — source-bootstrap BRICK 5: GNU Make 3.82 REBUILT by gcc-mesboot0 against glibc
//! (guix's make-mesboot). The chain's working make so far is make-mesboot0 (3.80, tcc-built, mes libc);
//! now that td has a real gcc + glibc, that make rebuilds GNU Make 3.82 with them — a glibc-linked make,
//! the one the gcc-mesboot1 (4.6.4) arc is built with. From the 229-byte seed: chain → gcc-mesboot0 →
//! make-mesboot (CC=<gcc-mesboot0>, binutils-mesboot0 as/ld/ar, glibc libc, LIBS=-lc -lnss_files
//! -lnss_dns -lresolv for static glibc). i686, static, serial. DURABLE: pinned-input (9 tarballs + 4
//! boot patches + make-3.82), no-guix (no /gnu/store in make), behavioral (make 3.82 parses a Makefile +
//! runs a recipe → BUILT), repro (byte-identical make). NOT a BUILD_GATE. gcc-mesboot1 (4.6.4, needs
//! gmp/mpfr/mpc) is next.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-make-mesboot",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        script: r##"
echo ">> bootstrap-make-mesboot: gcc-mesboot0 rebuilds GNU Make 3.82 against glibc — a glibc-linked make that does its job, guix-free + reproducible (source-bootstrap brick 5)"
sh tests/bootstrap-make-mesboot.sh
"##,
    }
}
