//! store-add-tree (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td
//! CANONICALLY restores a DIRECTORY TREE into its OWN store and registers it — the
//! RECURSIVE addToStore (the general write side, after the flat `store-add`), in pure
//! Rust, no daemon, NO guix. `td-builder store-add-recursive` computes the
//! content-addressed `source` path from the tree's recursive NAR sha256
//! (`make_store_path("source", …)` — the daemon's makeFixedOutputPath for
//! recursive-sha256, no references), restores the tree with `copy_canonical` (structure
//! + contents + the file EXECUTABLE bit + symlinks — the properties NAR captures; dir
//! perms / rw bits / mtimes are NAR-irrelevant), and registers it in a td store DB.
//!
//! Subject: a self-contained FIXTURE tree assembled in scratch — a nested dir, a plain
//! file, an executable file, and a symlink — so the gate controls every NAR-captured
//! property directly and needs no external tree. All-td-native / all-durable:
//! [DETERMINISM] re-interning the identical tree yields the IDENTICAL path (the path is
//! a pure function of the content). [ROUND-TRIP] the restored tree is NAR-byte-identical
//! to the source (exec bits + symlinks + nesting survive copy_canonical), cross-checked
//! by concrete restored-tree probes. [REGISTRATION] td's OWN reader reads back the path
//! + the tree's NAR hash. [DISCRIMINATION, load-bearing] a single-byte append AND an
//! exec-bit flip each MOVE the content-addressed path (and the append moves the registered
//! NAR hash) — the addressing is a real function of the bytes, not a constant. Needs
//! td-builder built, so it slots in the heavy pool.
//!
//! History: the guix-daemon differential this gate began as — interning the daemon's own
//! `%builder-source` tree (lowered with `guix repl … lower-object`) and asserting td's
//! path/NAR equals the daemon's — was the `lowering` guix-surface site retired here
//! (#310 / directive 6). The daemon-equality ORACLE is dropped; the DETERMINISM +
//! ROUND-TRIP + DISCRIMINATION assertions below cover the same property (a stable
//! content address that faithfully captures the tree) td-native — trading a REMOVABLE
//! guix-canonical cross-check (the byte-hash-vs-Guix oracle, removable per directive 4)
//! for a DURABLE discrimination the old gate never proved (it only matched the daemon for
//! one tree, never showed the address changes when the tree does). Called out in the PR
//! per directive 3.

use crate::gates::{GateDef, Pool, StoreMode};

// Native (typed-Rust) gate body (#318 axis 3): the bash was ported verbatim into
// `gate_bodies::store_add_tree`; `script: ""` marks it native, so the runner execs
// `td-builder gate-body store-add-tree` (as the stage0) under the same memory wrapper.
pub fn gate() -> GateDef {
    GateDef {
        name: "store-add-tree",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: "",
    }
}
