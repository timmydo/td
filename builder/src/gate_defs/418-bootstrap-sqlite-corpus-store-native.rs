//! bootstrap-sqlite-corpus-store-native — /td/store harness userland (#312): sqlite, the ladder's
//! store-DB parser oracle (store-register's `PRAGMA integrity_check` + ValidPaths/Refs byte-compare
//! role, and a member of the loop toolchain list), built from its RECIPE by td's OWN /td/store
//! toolchain — the bootstrap-hello/sed-corpus-store-native engine path (`td-builder build-recipe`
//! with the /td/store toolchain substituted for the lock's pinned gcc-toolchain-15.2.0, chained via
//! closure_multi/TD_EXTRA_DBS + multi-prefix sandbox staging), applied to the first #312 fan-out
//! tool. The sqlite3 binary links the /td/store glibc 2.41, references NO seed gcc-toolchain, and
//! runs in the own-root DRIVEN AS THE LADDER DRIVES IT: PRAGMA integrity_check = ok over a
//! td-WRITTEN store DB, ValidPaths reads back the interned glibc path (content-addressed, so
//! self-discriminating), a real SQL write/read round-trip → 42 on the ns tmpfs, and a garbage
//! non-DB is rejected (the oracle is not vacuous) — /gnu/store ABSENT. Seed provisioning is
//! guix-PROCESS-free, unlike the grandfathered hello/sed siblings: resolve-seed (td-subst, #311)
//! supplies the pinned lock closure and the warm seed capture content-scans the store directory.
//! `non_blocking` matches the corpus siblings until the seed-provisioning dependence retires
//! (#311/#350): on a host without the warm chain bricks or the pinned seed closure this red is
//! environmental, not a regression. Warm via the shared chain cache (#317); heavy. NOT a
//! BUILD_GATE (it builds its subject in-gate with the substituted toolchain, so it declares no
//! spec — the build-recipes prelude's pinned-toolchain pre-build would be a different artifact).

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-sqlite-corpus-store-native",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        // Typed artifact inputs (#353): the static-bash fixture from the lock's pinned
        // closure + the lock's gcc-toolchain entry (the lock-rewrite base) — resolved
        // by the runner.
        inputs: &[
            ArtifactInput {
                name: "bash-static",
                kind: InputKind::ClosureMember {
                    lock: "tests/sqlite-no-guix.lock",
                    root_stem: "bash",
                    member_stem: "bash-static",
                },
            },
            ArtifactInput {
                name: "gcc-toolchain",
                kind: InputKind::LockEntry {
                    lock: "tests/sqlite-no-guix.lock",
                    stem: "gcc-toolchain",
                },
            },
        ],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> bootstrap-sqlite-corpus-store-native: the /td/store toolchain (gcc 14.3.0 + binutils 2.44 + glibc 2.41, from the seed via the warm chain) builds sqlite 3.51.0 from its recipe via build-recipe — substituted for the lock's pinned gcc-toolchain-15.2.0; sqlite3 links /td/store glibc 2.41, no seed gcc-toolchain ref, runs in the own-root as the ladder's parser oracle (integrity_check + ValidPaths over a td-written store DB, SQL round-trip → 42, garbage rejected), /gnu/store ABSENT (#312 harness userland, first tool)"
sh tests/bootstrap-sqlite-corpus-store-native.sh
"##,
    }
}
