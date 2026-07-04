//! seed-subst — the loop realizes a MISSING pinned /gnu/store seed through td's OWN signed
//! substitute store (tools/resolve-seed.sh) instead of a `guix build` process (#311; unblocks
//! the guix-less runner, re #294 gap (b)). The first cutover is the loop's own host prelude:
//! tests/cache-lib.sh `provision_stage0` (the `td-builder check` + check-rung container-
//! provider prelude) now calls resolve-seed.sh — the retired `guix build <lock paths>` site is
//! DELETED, not bypassed, and a missing seed with no substitute store FAILS CLOSED (clear
//! message, no guix fallback). The producer half is tools/publish-seed-subst.sh: the daily
//! (ci/daily-full-suite.sh) captures the pinned seed closure by CONTENT-SCANNING the live
//! store bytes (tools/warm-seed.sh TD_SEED_DB=<dir> — zero /var/guix/db reads), subst-exports
//! every member (narinfo References = the closure edges the resolver walks), signs it
//! (trust anchor tests/td-subst.pub), and publishes into ~/.td/subst.
//!
//! [DURABLE behavioral] the REAL prelude (provision_stage0) resolves a seed lock whose root is
//! absent from a scratch seed root by FETCHING it ref-closed from the substitute store (ed25519
//! sig + StorePath==expected + NarHash verified, restored byte-identical), the fetched bash
//! RUNS, stage0 provisions — with a POISONED `guix` first on PATH (any guix process reds).
//! [DURABLE behavioral] the producer captures/export/signs the closure and is idempotent.
//! [DURABLE structural] the all-present warm path succeeds with a bogus substitute store.
//! [SELF-DISCRIMINATION] missing seed + no store -> FAIL-CLOSED with no guix process (red on
//! the retired guix-build fallback); wrong pinned key / wrong-StorePath substitute / empty
//! lock each red the resolve; the shared prelude carries no guix-build invocation anymore.
//!
//! Seed BYTES stay the guix-built pin (retired last, per the north star) — this removes the
//! guix PROCESS from seed realization. Uses the td-built td-subst binary from the td-subst
//! gate's cache (needs: td-subst), like feed-shared uses td-feed's. Not a BUILD_GATE.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "seed-subst",
        pools: &[Pool::Heavy],
        needs: &["td-subst"],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> seed-subst: a missing pinned /gnu/store seed is realized via td's OWN signed substitute store (resolve-seed.sh), ref-closed + verified, through the REAL host prelude with guix POISONED on PATH; missing+no-store FAILS CLOSED (no guix fallback); the producer (publish-seed-subst.sh) captures/exports/signs the closure content-scanned (#311)"
sh tests/seed-subst.sh
"##,
    }
}
