//! harness-subst — SHIP the /td/store harness to a guix-less runner via td-subst (#314). A runner
//! with no guix and an EMPTY .td-build-cache/harness can't BUILD the harness (that needs a guix
//! capture host), so `td-builder check check-harness` used to just FATAL — the circularity that kept
//! the cloud daily runner guix-dependent (#294). This gate proves the shipping mechanism end to end:
//! the daily EXPORTS + SIGNS the whole harness TREE-SET (store/ with content-addressed entries, the
//! loose /td/store/ld loader, and the rel + toolchain metadata) as ONE fixed-name substitute
//! (`td-harness`), and the guix-less runner's resolver (tools/resolve-harness.sh — the exact consumer
//! run_check_harness calls) FETCHES + VERIFIES + RESTORES it, or FAILS CLOSED.
//!
//! Trust = the ed25519 signature (pinned tests/td-subst.pub) + the signed NarHash + StorePath ==
//! td-harness; the harness is a content-addressed build output with no lock name to recompute, so the
//! fixed name + signature carry it (the daily republishes every green run). Fixture-based like the
//! sibling subst gates 358/359 (a harness-shaped tree whose runnable member is a real static bash): it
//! exercises the byte-agnostic export/fetch/restore path identically; the REAL busybox+make+gcc harness
//! running HARNESS-LOOP-OK from these bytes is proven by gate 420 + the daily's check-harness leg.
//!
//! Durable: FETCH RUNS (a binary from the fetched-not-built store runs), TREE-SET INTACT (loose ld +
//! rel + toolchain metadata round-trip byte-identically), FAIL CLOSED (cold store -> MISS, no dir),
//! and self-discrimination (wrong pinned key / wrong StorePath -> MISS). A BUILD_GATE like td-subst:
//! builds td-subst from source, ordered after the build-recipes phase.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "harness-subst",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        store: StoreMode::Shared,
        non_blocking: false,
        script: r##"
echo ">> harness-subst: the /td/store harness ships to guix-less runners via td-subst (#314) — the daily EXPORTS + SIGNS the whole tree as one td-harness substitute; tools/resolve-harness.sh (the consumer run_check_harness calls) FETCHES it (sig vs the pinned key + StorePath + NarHash verified), restores a runnable, metadata-intact, byte-identical tree, and FAILS CLOSED on a cold store / wrong key / wrong StorePath"
sh tests/harness-subst.sh
"##,
    }
}
