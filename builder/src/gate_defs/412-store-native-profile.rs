//! store-native-profile — prove `td-builder profile --store-native` assembles a profile of
//! LOGICAL /td/store symlinks that RESOLVE + RUN inside a store-ns own-root with /gnu/store
//! ABSENT: the .scm-free userspace ASSEMBLY mechanism (no guix operating-system). The tool is
//! bash-static (the cheap store-ns runner pattern); the guix-FREE /td/store-native userland the
//! toolchain builds (#192/#197) joins this same mechanism.
//! Heavy: builds the guix-free stage0 td-builder + runs a rootless userns (like store-ns 386).
//!
//! Native (#318 axis 3): the gate body is typed Rust in `gate_bodies::store_native_profile`;
//! `script: ""` marks it native, so the runner execs `td-builder gate-body store-native-profile`.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "store-native-profile",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[ArtifactInput {
            name: "bash-static",
            kind: InputKind::ClosureMember {
                lock: "tests/td-subst.lock",
                root_stem: "bash",
                member_stem: "bash-static",
            },
        }],
        store: StoreMode::Shared,
        non_blocking: false,
        script: "",
    }
}
