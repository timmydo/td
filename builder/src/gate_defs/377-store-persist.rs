//! store-persist — the LOOP builds a corpus package into a PERSISTENT /td/store + DB, and
//! a SEPARATE `td-builder` invocation SKIPS the rebuild by reading it back: the incremental
//! /td/store, build-into / read-back across builds, wired into the BUILD PATH (not a
//! test-only subcommand). Reuses the store-native corpus path (gate 416): from the seed
//! `bootstrap_modern_toolchain` builds the /td/store toolchain, then `td-builder
//! build-recipe` builds GNU sed 4.9 with it CANONICALLY at /td/store (TD_STORE_DIR) into a
//! persistent store P (TD_PERSIST_STORE/TD_PERSIST_DB). Invocation 1 = CACHE=miss +
//! build-into (merge_output_db); invocation 2 (fresh scratch) = CACHE=persist — the build
//! path finds sed valid in P (persistent_realization) and SKIPS the build; the sed READ BACK
//! FROM P runs in the own-root, /gnu/store ABSENT, transforming foo->bar. DURABLE (build-into,
//! skip/read-back, behavioral). guix only = the one-time seed capture + the seed toolchain
//! (§5, retired last); the build reads the td-owned seed DB, not /var/guix (no new guix surface).
//! Heavy (the /td/store toolchain from the seed); the build-recipes prelude runs → BUILD_GATES.
//! 
//! (Was PARKED between PR #291 and the #292 fix: realize_drv canonicalized every seed-store
//! candidate under the ACTIVE store dir, so this gate's /gnu/store lock roots missed the
//! index and the staged closure collapsed to the lock entries — coreutils' gmp dropped,
//! `expr` died on libgmp.so.10. Fixed by seed-canonical-prefix + recanonicalize_candidates.)

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "store-persist",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        // Typed artifact inputs (#353): the sed-lock pieces the script consumed by
        // hand — the build bash (the generated builders' shebang), brick8's
        // gcc-toolchain lock-rewrite base, and the static-bash own-root runner from
        // the pinned closure — resolved by the runner. (The $newlock coreutils read
        // stays: that lock is gate-GENERATED, not a pinned input.)
        inputs: &[
            ArtifactInput {
                name: "bash",
                kind: InputKind::LockEntry { lock: "tests/sed-no-guix.lock", stem: "bash" },
            },
            ArtifactInput {
                name: "gcc-toolchain",
                kind: InputKind::LockEntry { lock: "tests/sed-no-guix.lock", stem: "gcc-toolchain" },
            },
            ArtifactInput {
                name: "bash-static",
                kind: InputKind::ClosureMember {
                    lock: "tests/sed-no-guix.lock",
                    root_stem: "bash",
                    member_stem: "bash-static",
                },
            },
        ],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> store-persist: the loop builds a corpus package at /td/store into a persistent store + DB (build-into), and a SEPARATE invocation SKIPS the rebuild reading it back (CACHE=persist), running it own-root /gnu/store-absent — incremental /td/store, wired into the build path"
sh tests/store-persist.sh
"##,
    }
}
