//! seed-unpack — North-Star step 2 (CLAUDE.md), PR2: RESTORE a frozen seed tarball into a
//! td-owned store + register it, NO daemon and NO /gnu/store write. `td-builder seed-unpack`
//! extracts the tarball into a fresh store, verifies every tree is NAR-identical to the
//! manifest, and writes the store DB (ValidPaths + Refs) from the manifest. tests/seed-unpack.sh
//! captures hello's pinned bash closure, unpacks it, and asserts: the whole closure restores +
//! NAR-verifies (durable), every path is present + NAR-identical (durable), td's OWN
//! store-closure reads the COMPLETE closure back out of the unpacked DB (durable, no daemon),
//! and it == `guix gc -R` (removable oracle). The registered seed store is what a build stages
//! from with no guix install (PR3: build hello from it). td-builder is the guix-free stage0;
//! guix is only the capture source + the removable oracle. Heavy (stage0 + a real tar).

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "seed-unpack",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        store: StoreMode::Private, // cold by design (#317 audit): restores a frozen seed into a FRESH store and asserts registration
        non_blocking: true,
        script: r##"
echo ">> seed-unpack: td-builder restores a frozen seed tarball into a td store + registers it (NAR-verified, no daemon); td's reader reads the complete closure back out (North-Star step 2 PR2)"
sh tests/seed-unpack.sh
"##,
    }
}
