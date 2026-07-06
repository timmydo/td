//! store-add (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td PLACES
//! a path into its OWN store and REGISTERS it itself — the daemon's addToStore (the WRITE
//! side), in pure Rust, no daemon in td's write path. `td-builder store-add-text` computes
//! the addTextToStore path (`store::make_text_path`), WRITES the content into a td-owned
//! store dir as a canonical store file (regular, 0444), and registers it in a td store DB
//! (`store_db`). The gate asserts the path equals td's own `make_text_path` result, the
//! written bytes match the input, the file is canonical read-only, and td's registration,
//! read back with TD'S OWN reader (`store-query`, the #36 increment), records that path +
//! the NAR hash of what td wrote. NAR ignores mtime + the read/write perm bits, so store
//! identity is metadata-independent. Needs td-builder built, so it slots in the heavy pool.

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
