//! seed-tarball — North-Star step 2 (CLAUDE.md), PR1: CAPTURE a toolchain seed closure
//! into a frozen, portable tarball + manifest and prove the seed survives the tarball
//! byte-for-byte. tools/build-seed-tarball.sh tars the GC closure of a seed root with
//! td-builder's OWN store-closure + nar-hash (no daemon); tests/seed-tarball.sh extracts
//! it into a fresh dir and asserts every tree is NAR-IDENTICAL to the manifest (durable),
//! the manifest is closure-complete (durable), and td's closure == `guix gc -R` (removable
//! oracle). This is the capture half of "serve the toolchain seed from a tarball, not a
//! host guix"; `seed-unpack` (restore + register + build-from-seed) is the next PR. The
//! td-builder is the guix-free stage0; guix is only the capture SOURCE + the removable
//! closure oracle (no `guix build -e (system M)` packager). Heavy (stage0 + a real tar).

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "seed-tarball",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        // Typed artifact inputs (#353): resolved by the runner —
        // the body consumes TD_GATE_INPUT_*.
        inputs: &[
            ArtifactInput {
                name: "bash",
                kind: InputKind::LockEntry { lock: "tests/hello-no-guix.lock", stem: "bash" },
            },
        ],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> seed-tarball: capture a toolchain seed closure into a frozen tarball + manifest; the seed is NAR-identical after a tar round-trip (North-Star step 2 PR1)"
sh tests/seed-tarball.sh
"##,
    }
}
