//! store-backend (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). A td
//! STORE BACKEND for a BUILD OUTPUT — the capstone that composes the store stack into a
//! working, daemon-free backend. `td-builder store-add-output` PLACES a built output's tree
//! into a td-owned store at its output path and FULLY REGISTERS it (hash + narSize +
//! deriver + the output's references + the drv->output mapping — the daemon's post-build
//! registration), and then td's OWN tools SERVE it: store-query (the registration + the
//! references), store-verify (integrity re-hashed against the PLACED files), all with NO
//! daemon in any store operation.
//! R3 (guix-retirement ladder → #261): the SUBJECT is now td-BUILT (gate_bodies::store_subject —
//! a synthetic td-built subject and its closure CONTENT-SCANNED, so this gate runs with
//! guix OFF PATH — no `guix build [-d]`, no `guix gc`, no /var/guix read. The removable oracle
//! (the placed tree == the DAEMON's built output; store-query == the live /var/guix/db record;
//! references == `guix gc --references`; deriver/drv->output == the daemon's) is DROPPED per
//! CLAUDE.md directive 3 (called out in the PR). In its place, td-INTERNAL consistency over a
//! td-built subject: (1) the placed tree is NAR-identical to the SOURCE staged tree; (2)
//! store-query info == the re-derived hash+narSize of that tree; (3) store-query references ==
//! the subject's DIRECT references as INDEPENDENTLY computed by store-register over the closure (two
//! separate scan paths agree); (4) store-verify passes against td's own placed files; (5) the
//! deriver + drv->output mapping td records is exactly (td-assembled .drv) -> out -> the output.
//! Boundary: td writes only its OWN scratch store/DB; the host store is untouched. Needs
//! td-builder + the corpus build → heavy pool + the build-recipes prelude.

use crate::gates::{GateDef, Pool};

// Native (typed-Rust) gate body (#318 axis 3): the bash was ported verbatim into
// `gate_bodies::store_backend`; `script: ""` marks it native, so the runner execs
// `td-builder gate-body store-backend` (as the stage0) under the same memory wrapper.
pub fn gate() -> GateDef {
    GateDef {
        name: "store-backend",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        inputs: &[],
        non_blocking: false,
        script: "",
    }
}
