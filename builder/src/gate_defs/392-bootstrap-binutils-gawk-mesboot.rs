//! bootstrap-binutils-gawk-mesboot — source-bootstrap BRICK 5: the gcc-mesboot1 (GCC 4.6.4, C++)
//! toolchain rebuilds GNU Binutils 2.20.1a AND builds GNU awk 3.1.8 (guix's binutils-mesboot +
//! gawk-mesboot) — the two tools the next C library (glibc-mesboot 2.16.0) needs (binutils built by the
//! c++ gcc; a real GNU awk, since glibc 2.16.0's versions.awk is too complex for the seed's gash awk).
//! From the 229-byte seed: chain → gcc-mesboot1 → {binutils-mesboot, gawk-mesboot}. i686, static, serial.
//! DURABLE: pinned-input (chain + 5 boot patches + gcc-4.6.4/gcc-g++/gmp/mpfr/mpc/gawk-3.1.8), no-guix
//! (no /gnu/store in as/ld/gawk), behavioral (as+ld assemble+link+run C → 42; gawk processes text + sums
//! → 42), repro (byte-identical as+ld+gawk). NOT a BUILD_GATE. glibc-mesboot (2.16.0) then gcc-mesboot
//! (GCC 4.9, the final mesboot gcc) are next.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-binutils-gawk-mesboot",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        script: r##"
echo ">> bootstrap-binutils-gawk-mesboot: the gcc-mesboot1 toolchain rebuilds GNU Binutils 2.20.1a + builds GNU awk 3.1.8 — guix-free + reproducible (source-bootstrap brick 5)"
sh tests/bootstrap-binutils-gawk-mesboot.sh
"##,
    }
}
