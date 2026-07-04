//! toolchain-x86_64-input-addressed — the x86_64 /td/store toolchain (cross binutils-2.44 +
//! cross gcc-14.3.0 + x86_64 glibc-2.41 + libgcc_s, built from the seed by gate
//! rust-x86_64-runtime-store-native via tests/x86_64-cross-fns.sh, #201) gets a STABLE
//! INPUT-ADDRESSED key — the x86_64 parallel of toolchain-input-addressed (#204, i686). The
//! toolchain is not byte-reproducible, so store-add-recursive's content-addressed path varies
//! build-to-build; tests/td-toolchain-x86_64.lock + `td-builder toolchain-key/toolchain-path`
//! derive the path from the DECLARED inputs (a pure function), so it is identical across
//! rebuilds and predictable from the lock — the prereq for fetching the x86_64 toolchain
//! instead of the ~90-min from-seed rebuild (the rust compile/userland rungs 3/4).
//! DURABLE, td-native end to end (no guix oracle): pinned-sync (lock pins == seed pins),
//! arch-parity (shares i686's exact source set; only name/recipe-rev/component differ),
//! distinct-key (arch re-keys -> no collision with i686), stable-key (deterministic, distinct
//! component paths), load-bearing (recipe-rev + a pin move the addressing), behavioral (a real
//! binary placed at the x86_64-keyed path runs in the store-ns own-root, /gnu/store absent).
//! Heavy: builds the guix-free stage0 td-builder + runs a rootless userns (like #204) — NOT a
//! ~90-min toolchain build, NOT a BUILD_GATE.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "toolchain-x86_64-input-addressed",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: false,
        script: r##"
echo ">> toolchain-x86_64-input-addressed: the x86_64 /td/store toolchain gets a STABLE input-addressed key (td-toolchain-x86_64.lock + toolchain-key/path) — sharing i686's source set with ARCH as the sole discriminator, predictable from the lock across non-reproducible rebuilds; a real binary placed there runs, /gnu/store absent (the x86_64 parallel of #204 — the x86_64 td-subst chain-caching prereq)"
sh tests/toolchain-x86_64-input-addressed.sh
"##,
    }
}
