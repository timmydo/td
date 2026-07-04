//! seed-build — North-Star step 2 (CLAUDE.md), PR3, the payoff: BUILD hello from the UNPACKED
//! SEED, with NO guix install. tests/seed-build.sh captures hello's full build closure (its lock
//! inputs + the stage0 builder's runtime) into a frozen tarball, `seed-unpack`s it into a fresh
//! td store, then `td-builder build-recipe` builds hello passing the unpacked seed DB as its ONLY
//! store DB (TD_SEED_STORE/TD_SEED_DB) — /var/guix + the live /gnu/store out of the build path.
//! A missing seed path can't fall back to guix, so a green build proves the tarball is a
//! self-sufficient seed. Asserts: hello builds + runs from the seed (durable behavioral), every
//! input stages FROM the unpacked store not /gnu/store (durable structural), and the seed-built
//! hello is the SAME store path as the guix-seed build (removable oracle — own, then diverge).
//! guix/Guile scrubbed from PATH; guix is only the one-time capture source + the oracle. Heavy
//! (stage0 + ~660M seed tar + a real hello build) → BUILD_GATES + HEAVY_GATES.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "seed-build",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        store: StoreMode::Private, // cold by design (#317 audit): builds hello from the unpacked seed alone (no warm store)
        script: r##"
echo ">> seed-build: build hello from the unpacked seed tarball (its only store DB) — /var/guix out of the path; td builds with NO guix install (North-Star step 2 PR3)"
sh tests/seed-build.sh
"##,
    }
}
