//! bash-x86_64-store-native — /td/store harness userland (NO GUIX BYTES), re #312: GNU bash 5.2.37
//! — the shell the gate RUNNER itself runs every gate body under (builder/src/gates.rs executes
//! each `script` as one `bash -c <body>`) — built FROM upstream source (td-fetch, sha-pinned) by
//! the from-seed /td/store x86_64 toolchain (reused from the x86_64 gate as a function library,
//! fetched via warm-subst or built from the 229-byte seed), DYNAMIC vs the /td/store glibc 2.41
//! (interp = /td/store/ld), interned at /td/store, and RUN in the store-ns own-root the way the
//! runner drives it — `bash -c` of a real multi-command body using bash-only features (arrays,
//! [[ ]], ${v^^}) that busybox ash cannot — with /gnu/store ABSENT. This brings the ladder's own
//! interpreter into td's store, shrinking the guix `guix shell` prelude (re #312). Durable
//! supply-chain/provenance/no-guix/behavioral/structural legs; verified-red in-gate (without the
//! interp relink the own-root run fails). HEAVY (~90 min from seed, ~15 with the warm-subst
//! toolchain fetch; directive 1 — no cache). NOT a BUILD_GATE. Mirrors gate 420
//! (userland-x86_64-store-native): same toolchain obtain + _xbin scaffolding + intern/own-root
//! pattern (the shared prologue is #365 dedup territory).

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bash-x86_64-store-native",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        // Typed artifact inputs (#353): resolved by the runner — the shared
        // x86_64 libs consume TD_GATE_INPUT_{COREUTILS,BASH_STATIC}.
        inputs: &[
            ArtifactInput {
                name: "coreutils",
                kind: InputKind::LockEntry { lock: "tests/td-subst.lock", stem: "coreutils" },
            },
            ArtifactInput {
                name: "bash-static",
                kind: InputKind::ClosureMember {
                    lock: "tests/hello-no-guix.lock",
                    root_stem: "bash",
                    member_stem: "bash-static",
                },
            },
        ],
        store: StoreMode::Shared,
        non_blocking: false,
        script: r##"
echo ">> bash-x86_64-store-native: GNU bash 5.2.37 built from upstream source by the /td/store toolchain, dynamic vs /td/store glibc, run in the store-ns own-root under `bash -c` (as the gate runner runs every gate body) — NO /gnu/store, no guix bytes (re #312)"
sh tests/bash-x86_64-store-native.sh
"##,
    }
}
