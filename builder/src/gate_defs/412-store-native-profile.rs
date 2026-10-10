//! store-native-profile — prove `td-builder profile --store-native` assembles a profile of
//! LOGICAL /td/store symlinks that RESOLVE + RUN inside a store-ns own-root with /gnu/store
//! ABSENT: the .scm-free userspace ASSEMBLY mechanism (no guix operating-system). The package
//! is the stage0 td-builder's item plus a relative link to it (two profile entries), and the
//! profiled td-builder's `gate-probe own-root` resolves both on the profile's bin and runs;
//! the guix-FREE /td/store-native userland the toolchain builds (#192/#197) joins this same mechanism.
//! Heavy: builds the guix-free stage0 td-builder + runs a rootless userns (like store-ns 386).
//!
//! Native (#318 axis 3): the gate body is typed Rust in `gate_bodies::store_native_profile`;
//! the runner execs `td-builder gate-body store-native-profile`.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "store-native-profile",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        non_blocking: false,
    }
}
