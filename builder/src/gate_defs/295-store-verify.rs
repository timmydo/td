//! store-verify (DESIGN §7.1; td-store-db track — begin replacing guix-daemon). td
//! VERIFIES a store's integrity ITSELF — the daemon's `guix gc --verify --check-contents`,
//! in pure Rust, no daemon. `td-builder store-verify DB STORE-ROOT` reads the recorded
//! registration from a td store DB (`store_db_read`, #36) and re-NAR-hashes each registered
//! path at STORE-ROOT/<basename>, flagging (exit 1) any path whose content no longer matches
//! its recorded `hash`.
//! R3 (guix-retirement ladder → #261): the SUBJECT is now td-BUILT (gate_bodies::store_subject —
//! synthetic td-built subject staged into a td-OWNED store and its closure
//! CONTENT-SCANNED, so this gate runs with guix OFF PATH — no `guix build`, no `guix gc`, no
//! /var/guix read. The removable DAEMON DIFFERENTIAL (leg A used to prove td.db records the
//! live /var/guix/db hashes and then verify /gnu/store against them) is DROPPED per CLAUDE.md
//! directive 3 (called out in the PR); in its place the CORRUPTION-DETECTION feature now runs
//! against the REAL td-built closure, a stronger test than the old synthetic-only probe. Three
//! legs: (A) td-verify PASSES over the intact td-built closure it registered; (B) a one-byte
//! corruption of a closure member is DETECTED (verify exits nonzero); (C) an independent flat
//! probe added by `store-add-text` verifies OK, then a one-byte corruption of it is DETECTED.
//! Boundary: td READS + writes only its OWN scratch store/DB/probe — host infra stays
//! immutable. Needs td-builder + the corpus build → heavy pool + the build-recipes prelude.

use crate::gates::{GateDef, Pool, StoreMode};

// Native (typed-Rust) gate body (#318 axis 3): the bash was ported verbatim into
// `gate_bodies::store_verify`; `script: ""` marks it native, so the runner execs
// `td-builder gate-body store-verify` (as the stage0) under the same memory wrapper.
pub fn gate() -> GateDef {
    GateDef {
        name: "store-verify",
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
