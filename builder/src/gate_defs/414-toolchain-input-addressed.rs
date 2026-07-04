//! toolchain-input-addressed — task 2a: the /td/store modern toolchain (gcc-14.3.0 +
//! binutils-2.44 + glibc-2.41, gate 412) gets a STABLE input-addressed key. The toolchain is
//! not byte-reproducible (cc1 stamp, ar/install mtimes), so `store-add-recursive`'s
//! content-addressed path varies build-to-build and a td-subst consumer can't name what to
//! fetch. tests/td-toolchain.lock + `td-builder toolchain-key/toolchain-path` derive the path
//! from the toolchain's DECLARED inputs (a pure function of inputs), so it is identical across
//! rebuilds and predictable from the lock — the prereq for td-subst chain-caching (2b/2c).
//! Durable, td-native end to end (no guix oracle): pinned-sync (lock pins == seed pins),
//! stable-key (deterministic, distinct component paths), content-indep (same key+different
//! bytes -> same path, vs content-addressed which splits), load-bearing (a pin moves the
//! path), behavioral+structural (a real binary at an input-addressed path runs in the
//! store-ns own-root, /gnu/store absent). Heavy: builds the guix-free stage0 td-builder +
//! runs a rootless userns (like store-native-profile). NOT a BUILD_GATE.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "toolchain-input-addressed",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        // Typed artifact input (#353): the runnable static-bash fixture from
        // hello's pinned closure — resolved by the runner, no lock-grepping or
        // store-closure-scan in the body.
        inputs: &[ArtifactInput {
            name: "bash-static",
            kind: InputKind::ClosureMember {
                lock: "tests/hello-no-guix.lock",
                root_stem: "bash",
                member_stem: "bash-static",
            },
        }],
        store: StoreMode::Shared,
        non_blocking: false,
        script: r##"
echo ">> toolchain-input-addressed: the /td/store modern toolchain gets a STABLE input-addressed key (td-toolchain.lock + toolchain-key/path) — identical across non-reproducible rebuilds, predictable from the lock; a real binary placed there runs, /gnu/store absent (task 2a — td-subst chain-caching prereq)"
sh tests/toolchain-input-addressed.sh
"##,
    }
}
