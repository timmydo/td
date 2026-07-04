//! chain-cache — the #317 FLIPPED gate-state default is correct: a Shared gate's chain
//! brick builds ONCE machine-wide, every reuse is NAR-verified (`$TB nar-hash` against the
//! sentinel recorded at build time), a POISONED cache entry is rejected and rebuilt (never
//! consumed), and a cold run — what the runner wires for a `StoreMode::Private` gate
//! (TD_CHECK_CHAIN_CACHE force-cleared) — neither reads nor writes the cache. Plus whole-key
//! GC (#326): chain_cache_init sweeps a key whose `last-used` stamp has aged past the
//! threshold, while the LIVE key still NAR-verifies + cache-hits and a key whose exclusive
//! flock is held (a concurrent build) is NEVER swept — bounding the machine-wide cache
//! against ENOSPC as recipe/pin/channel changes mint fresh multi-GB keys. Drives the REAL
//! library the bootstrap chain sources (tests/chain-cache-lib.sh) through the same
//! hit/build/save/init entry points `bootstrap_modern_toolchain` uses per brick, with the
//! real stage0 verifier. Heavy+Engine: the engine smoke covers it because the runner-side
//! wiring (gates.rs run_gate) and this lib change together.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "chain-cache",
        pools: &[Pool::Heavy, Pool::Engine],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        // Private: the gate must exercise its OWN throwaway cache, never the machine-wide
        // one (it poisons entries on purpose).
        store: StoreMode::Private,
        non_blocking: false,
        script: r##"
echo ">> chain-cache: warm bricks build once + NAR-verified reuse; poisoned entries rejected; Private/cold runs never touch the cache (#317); whole-key GC sweeps stale keys, spares live + flock-held (#326)"
sh tests/chain-cache.sh
"##,
    }
}
