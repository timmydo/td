//! toolchain-x86_64-input-addressed — the x86_64 /td/store toolchain (cross binutils-2.44 +
//! cross gcc-14.3.0 + x86_64 glibc-2.41 + libgcc_s, built from the seed via the
//! recipe graph and validated by recipe-owned checks) gets a STABLE
//! INPUT-ADDRESSED key — the x86_64 parallel of toolchain-input-addressed (#204, i686). The
//! toolchain is not byte-reproducible, so store-add-recursive's content-addressed path varies
//! build-to-build; tests/td-toolchain-x86_64.lock + `td-builder toolchain-key/toolchain-path`
//! derive the path from the DECLARED inputs (a pure function), so it is identical across
//! rebuilds and predictable from the lock — the prereq for fetching the x86_64 toolchain
//! instead of the ~90-min from-seed rebuild (the rust compile/userland rungs 3/4).
//! DURABLE, td-native end to end (no guix oracle): pinned-sync (lock pins == recipe source pins),
//! arch-parity (shares i686's exact source set; only name/recipe-rev/component differ),
//! distinct-key (arch re-keys -> no collision with i686), stable-key (deterministic, distinct
//! component paths), load-bearing (recipe-rev + a pin move the addressing), behavioral (a real
//! binary placed at the x86_64-keyed path runs in the store-ns own-root, /gnu/store absent).
//! Heavy: builds the guix-free stage0 td-builder + runs a rootless userns (like #204) — NOT a
//! ~90-min toolchain build, NOT a BUILD_GATE.
//!
//! Native (#318 axis 3): the gate body is typed Rust in
//! `gate_bodies::toolchain_x86_64_input_addressed`; `script: ""` marks it native, so the runner
//! execs `td-builder gate-body toolchain-x86_64-input-addressed`.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "toolchain-x86_64-input-addressed",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: false,
        specs: &[],
        // The runnable static fixture is the loop's td-built busybox, resolved
        // from PATH in the body (gate_bodies::busybox_pkg_dir) — no declared
        // guix-lock input.
        non_blocking: false,
        script: "",
    }
}
