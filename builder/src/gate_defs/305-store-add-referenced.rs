//! store-add-referenced (DESIGN §7.1; td-store-db track — begin replacing guix-daemon).
//! td ADDS a path WITH references to its OWN store — the daemon's addToStore/addTextToStore
//! with a references set (after the no-reference flat #38 + recursive #41 adds), in pure
//! Rust, no daemon. `td-builder store-add-referenced` computes the content-addressed path
//! with the references FOLDED INTO THE TYPE (`make_text_path`: `text:<sorted refs>` — the
//! daemon's makeTextPath/makeType), WRITES the content into a td-owned store (canonical 0444
//! file), and REGISTERS the path with its `Refs` to the referenced paths. The canonical
//! referenced content-addressed item is a `.drv` (referenced by its input drvs/srcs).
//! R3 (guix-retirement ladder → #261): the subject `.drv` is now the one td ASSEMBLED
//! (gate_bodies::store_subject — assemble-recipe, guix/Guile off PATH), NOT `guix build -d`, and
//! its references are read with `td-builder drv-refs` (parse inputDrvs ∪ inputSrcs), NOT `guix
//! gc --references`. So this gate runs with guix OFF PATH. The removable guix oracle (the
//! stored `.drv` byte-identical to the DAEMON's own + references == `guix gc --references`) is
//! DROPPED per CLAUDE.md directive 3 (called out in the PR); in its place a genuine ROUND-TRIP:
//! the references RECOVERED from the `.drv` bytes by `drv-refs` (parse — a DIFFERENT provenance
//! from the recipe inputs the ASSEMBLER folded in at build time) fold — through the shared
//! make_text_path — back to the SAME store path the assembler produced (drop a ref and the path
//! diverges, so this proves drv-refs recovers the exact folded set). Plus: the stored `.drv` is NAR-identical
//! to the source, and td registers EXACTLY the parsed references (read back by td's own
//! `store-query references`). Boundary: td writes only its OWN scratch store/DB. Needs
//! td-builder + the corpus build → heavy pool + the build-recipes prelude.

use crate::gates::{GateDef, Pool, StoreMode};

// Native (typed-Rust) gate body (#318 axis 3): the bash was ported verbatim into
// `gate_bodies::store_add_referenced`; `script: ""` marks it native, so the runner execs
// `td-builder gate-body store-add-referenced` (as the stage0) under the same memory wrapper.
pub fn gate() -> GateDef {
    GateDef {
        name: "store-add-referenced",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: "",
    }
}
