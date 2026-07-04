//! bootstrap-patch — source-bootstrap BRICK 5 (gcc toolchain), make-driven rung. From the 229-byte
//! seed, td builds Mes + MesCC + tcc + make (bricks 0-4 + the make rung), then the tcc-built GNU Make
//! compiles GNU patch 2.5.9 IN the loop sandbox. This clears the make-in-sandbox blocker: make's SHELL
//! makefile-variable defaults to /bin/sh (absent in the sandbox) and make ignores the SHELL env var,
//! so recipes segfaulted; the fix is `make SHELL=<curated sh>` (guix gets /bin/sh from gash). patch
//! also takes guix's pch.c "avoid another segfault" workaround. i686, static. Source td-fetched
//! (seed/sources/patch-2.5.9.lock). DURABLE: pinned-input (5 tarballs == locks), no-guix (no
//! gcc/guile/guix; no /gnu/store in patch), behavioral (make builds patch; patch runs + applies a
//! diff), repro (byte-identical). NOT a BUILD_GATE. binutils-mesboot0 (patch-applied + make) is next.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-patch",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: false,
        script: r##"
echo ">> bootstrap-patch: the tcc-built GNU Make compiles GNU patch 2.5.9 in the sandbox (SHELL override clears the no-/bin/sh segfault) — guix-free + reproducible (source-bootstrap brick 5)"
sh tests/bootstrap-patch.sh
"##,
    }
}
