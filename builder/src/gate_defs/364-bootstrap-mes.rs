//! bootstrap-mes — source-bootstrap BRICK 2 (north star: no guix BYTES). From the 229-byte stage0
//! seed, td builds M2-Planet + mescc-tools (brick 1) and drives them over the GNU Mes RELEASE
//! SOURCE — the pinned mes-0.27.1.tar.gz (seed/sources/mes-*.lock), td-fetched (not vendored, not
//! guix-fetched) in check.sh's prelude into .td-build-cache/sources/ — to compile + link a working
//! GNU Mes Scheme interpreter, mes-m2 — guix-free.
//! 
//! The driver is a STRUCTURED Rust recipe — `td-builder bootstrap-recipe mes`
//! (builder/src/bootstrap.rs, rust-migration C2): the old shell tests/bootstrap-mes.sh was ported
//! to a typed Recipe (Pin::Source; the kaem.run input-list extraction + the M2P/blood-elf/M1/hex2
//! chain) + the shared leg runner and DELETED — no shell oracle kept (this is all-durable; there
//! is no guix oracle). Own-then-diverge: the Rust-built mes-m2 was proven BYTE-IDENTICAL to the
//! old shell-built one (sha 203e5516…) before the shell was removed. ALL-DURABLE:
//! [pinned-input] the warmed tarball matches the lock sha256 — built from the exact pinned bytes;
//! [no-guix]    the whole chain runs env-cleared; no /gnu/store byte in mes-m2;
//! [behavioral] the seed-built mes-m2 evaluates Scheme (display + arithmetic) from the Mes
//! module tree — a real interpreter, not just a linked ELF;
//! [repro]      two independent mes builds yield a byte-identical mes-m2.
//! Standalone (static seed tools + ~minutes of M2-Planet/M1/hex2) — NOT a BUILD_GATE. Brick 3
//! bootstraps tinycc from mes; bricks 4-5 reach gcc/glibc at /td/store.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-mes",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> bootstrap-mes: the structured Rust mes recipe builds GNU Mes (mes-m2) and proves it evaluates Scheme, guix-free + reproducible (source-bootstrap brick 2)"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
test -x "$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
"$tb" bootstrap-recipe mes
"##,
    }
}
