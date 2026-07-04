//! userland-x86_64-store-native — host-sandbox-stage0 inc2 (NO GUIX BYTES): the guix-less
//! daily-suite captured set's C userland — busybox 1.37.0 + GNU make 4.4.1 — built FROM
//! upstream source (td-fetch, sha-pinned) by the from-seed /td/store x86_64 toolchain (reused
//! from the x86_64 gate as a function library), DYNAMIC vs the /td/store glibc 2.41 (interp =
//! /td/store/ld), interned at /td/store, and RUN in the store-ns own-root with /gnu/store
//! ABSENT. busybox = a POSIX userland (surfaces GNUisms); make = the explicit build driver.
//! It ALSO stages the C toolchain {binutils,gcc,glibc} into the persisted harness (Increment 3)
//! + a manifest, so the guix-free `./check.sh check-harness` loop COMPILES + runs real software,
//! not just drives text. Durable supply-chain/provenance/no-guix/structural/behavioral legs;
//! verified-red in-gate (without the interp relink the own-root run fails). HEAVY (~90 min from
//! seed, ~15 with the warm-subst toolchain fetch; directive 1 — no cache). NOT a BUILD_GATE.
//! (td-builder, the engine, joins the set via rust-store-native rung 3; this proves the
//! busybox+make + staged-compiler half.)

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "userland-x86_64-store-native",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        non_blocking: false,
        script: r##"
echo ">> userland-x86_64-store-native: busybox + GNU make built from upstream source by the /td/store toolchain, dynamic vs /td/store glibc, run in the store-ns own-root — NO /gnu/store, no guix bytes"
sh tests/userland-x86_64-store-native.sh
"##,
    }
}
