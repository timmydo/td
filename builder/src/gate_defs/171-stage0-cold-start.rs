//! stage0-cold-start — a COLD stage0 placement needs NO guix state (#313). Gate 170
//! proved td-builder needs no guix to be COMPILED; this gate proves the PLACEMENT half:
//! `td-builder stage0-place` (builder/src/stage0.rs — the one entry point every stage0
//! consumer goes through: the gate bodies' PlacedStage0, the check prelude, the daemon-ensure
//! probe) places a stage0 from a cold cache with guix's private state HIDDEN. Before #313,
//! store-add-builder hard-read /var/guix/db/db.sqlite as its reference-scan seed, so any
//! cold start (or any builder/ edit — a new fingerprint) FATALed on a guix-less host. Now
//! the reference-scan candidates come from a readdir of the seed store DIRECTORY
//! (scan_candidate_index, the #267 content-scan pattern), and an absent dir contributes
//! nothing.
//!
//! The stage0 builder is now musl-STATIC (builder/src/stage0.rs), so it embeds NO external
//! store references at all — its recorded closure is SELF-ONLY by construction. stage0_place
//! therefore scans an EMPTY seed dir and records just the builder itself. That self-only
//! closure IS the #469 no-leak property: a static builder drags no host runtime lib dir
//! (nor the +x `libasan.la` libtool archive beside it, #468) into the sandbox.
//!
//! Per the differential+durable discipline:
//! [DURABLE behavioral] with /var/guix bind-mounted EMPTY in a private mount ns, a cold
//! `td-builder stage0-place` places a stage0 that RUNS its sentinel.
//! [DURABLE no-drift] the cold placement is IDENTICAL to the warm guix-host placement:
//! same canonical path, same builder.db closure — and that closure is SELF-ONLY (exactly
//! the one canonical builder path, no external ref), the musl-static no-leak invariant.
//! [DURABLE guix-less arm] store-add-builder with an ABSENT seed dir (a truly guix-less
//! host: no /gnu/store at all) still places, recording a self-only closure — the arm the
//! rustup/system-cc cold start takes.
//! [DURABLE fail-loud] a PRESENT-but-unreadable seed dir (a regular file, not a
//! directory) ERRORS instead of silently placing a refless builder — the absent-dir
//! tolerance must not degrade into swallowing a misconfigured seed (a refless placement
//! would poison the closure and surface only as an opaque build failure).
//! [DURABLE self-discrimination] the reference-scan mechanism is load-bearing, proven
//! independently of the (now refless) builder: a probe tree embedding a SYNTHETIC store
//! path records NO ref with the seed dir absent and DOES record it when a controlled seed
//! dir holding the matching entry is passed — the readdir candidate source drives the scan.
//!
//! The full guix-less VM run on a host with no guix at all is the arc this unblocks;
//! this gate pins its cold-start contract inside the loop,
//! where a guix host can still A/B the warm baseline.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "stage0-cold-start",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        non_blocking: true,
    }
}
