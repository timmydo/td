//! store-add (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td PLACES
//! a path into its OWN store and REGISTERS it itself — the daemon's addToStore (the WRITE
//! side), in pure Rust, no daemon in td's write path. `td-builder store-add-text` computes
//! the addTextToStore path (`store::make_text_path`), WRITES the content into a td-owned
//! store dir as a canonical store file (regular, 0444), and registers it in a td store DB
//! (`store_db`). The differential (daemon = oracle, prime directive 4): the SAME bytes
//! added via the daemon's addTextToStore RPC (`store-add`, #27) — which writes the file to
//! /gnu/store and returns the path — give (a) the IDENTICAL store path, and (b) a store
//! file that is byte-identical (by NAR hash) to the one td wrote; and td's registration,
//! read back with TD'S OWN reader (`store-query`, the #36 increment), records that path +
//! the NAR hash of what td wrote. The daemon's OWN store file is the oracle (not the DB:
//! a freshly-added path sits in the daemon's WAL, invisible to an immutable db.sqlite
//! read; the on-disk store file is the direct, WAL-free oracle and the stronger claim —
//! td's store bytes == the daemon's). NAR ignores mtime + the read/write perm bits, so
//! store identity is metadata-independent. Boundary: td writes only its OWN scratch
//! store/DB and READS the daemon's store file; the daemon RPC adds a GC-able probe path
//! (as the existing store-add/drv-add gates do) purely as the oracle — host infra stays
//! immutable. Needs td-builder built, so it slots in the heavy pool.

use crate::gates::{GateDef, Pool, StoreMode};

// Native (typed-Rust) gate body (#318 axis 3): the bash was ported verbatim into
// `gate_bodies::store_add`; `script: ""` marks it native, so the runner execs
// `td-builder gate-body store-add` (as the stage0) under the same memory wrapper.
pub fn gate() -> GateDef {
    GateDef {
        name: "store-add",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: "",
    }
}
