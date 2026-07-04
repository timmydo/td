//! store-register (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td both
//! WRITES and READS the store SQLite DB for a TD-BUILT artifact's FULL CLOSURE itself — the
//! daemon's `ValidPaths`/`Refs`/`DerivationOutputs` authority AND its store-query role, in
//! pure Rust. `td-builder store-register` scans EVERY path in the closure (NAR hash + size +
//! reference scan, the `build` machinery) and writes the SQLite FILE FORMAT directly (the
//! `store_db` module: header + table b-tree leaf pages + the record/varint encoding, zero-dep)
//! — the real replacement of the daemon's libsqlite, NO `sqlite3` engine writing it.
//! `td-builder store-query` then READS that DB back with td's OWN pure-Rust SQLite reader
//! (`store_db_read`) — NO sqlite3 engine and NO daemon in td's store-query path.
//! R3 (guix-retirement ladder → #261): the SUBJECT is now td-BUILT (gate_bodies::store_subject —
//! hello via build-recipe, cache-hit off the build-recipes prelude) staged into a td-OWNED
//! store, and its closure is CONTENT-SCANNED, so this gate runs with guix OFF PATH — no `guix
//! build [-d]`, no `guix gc`, no /var/guix read. The removable DAEMON CONTENT ORACLE (the
//! live /var/guix/db comparison of every path's hash/narSize, the full Refs relation and the
//! drv->output) is DROPPED per CLAUDE.md directive 3 (called out in the PR). What remains is
//! STRONGER-still self-consistency over a td-built subject: td writes a store DB that `sqlite3`
//! confirms is structurally valid (`PRAGMA integrity_check` = ok — sqlite3 is a parser oracle,
//! not guix), and whose registration — as answered by TD'S OWN READER — reads back
//! BYTE-IDENTICAL to sqlite3 reading the same bytes for (1) every closure path's hash+narSize
//! and (2) the full inter-path Refs relation; and a deriver that IS itself a closure member is
//! registered ONCE (no duplicate ValidPaths row). Boundary: td writes only its OWN scratch
//! store/DB; the host store is untouched. Needs td-builder + the corpus build → heavy pool +
//! the build-recipes prelude.

use crate::gates::{GateDef, Pool, StoreMode};

// Native (typed-Rust) gate body (#318 axis 3): the bash was ported verbatim into
// `gate_bodies::store_register`; `script: ""` marks it native, so the runner execs
// `td-builder gate-body store-register` (as the stage0) under the same memory wrapper.
pub fn gate() -> GateDef {
    GateDef {
        name: "store-register",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: "",
    }
}
