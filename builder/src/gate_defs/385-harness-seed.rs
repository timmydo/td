//! harness-seed — HARNESS FROM A SEED (host-sandbox-stage0 inc2a, North-Star): the loop
//! CONTAINER stands up with NO guix process and NO host /gnu/store — the substrate a
//! guix-less cloud VM needs to run ci/daily-full-suite.sh. rust-seed (#134) proved td BUILDS
//! its engine from a seed but ran on a guix host (store present); this closes that gap —
//! capture the loop toolchain (make/bash/coreutils/…) into a seed, then enter td's
//! host-sandbox with the seed bound AT /gnu/store and the host store + /var/guix NOT bound
//! (`--store-from`/`--no-daemon`) and run the toolchain inside. Durable behavioral
//! (the toolchain runs) + structural (/gnu/store is the seed, guix unresolvable, /var/guix
//! absent) legs + a removable guix oracle (seed versions == host-store versions). guix is
//! only the one-time capture SOURCE (run on a guix host, like rust-seed/warm-seed). HEAVY +
//! BUILD_GATE so it slots after the build-recipes fan-out (it reuses the warm stage0).

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "harness-seed",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        script: r##"
echo ">> harness-seed: td's loop container stands up from a SEED alone — host /gnu/store + the guix daemon absent, guix off PATH — and the loop toolchain runs inside it (the guix-less VM substrate)"
sh tests/harness-seed.sh
"##,
    }
}
