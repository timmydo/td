//! toolchain-subst-default — the loop FETCHES the lock-keyed /td/store toolchain by DEFAULT
//! (tools/resolve-toolchain.sh) instead of rebuilding the ~18-rung from-seed chain (~90 min).
//! "Loop substitutes too" (human, 2026-06-28). Builds on the stable input-addressed key (#204)
//! and the lock-keyed publish->fetch leg (#207); the new bits are a PERSISTENT signed
//! substitute store keyed by tests/td-toolchain.lock, the consumer-DEFAULT resolver a real
//! bootstrap gate sources (tools/resolve-toolchain.sh: fetch-by-default, FALL BACK to from-seed
//! on any miss), and the daily-suite PUBLISHER (tools/publish-toolchain-subst.sh: export + sign
//! the lock-keyed toolchain into that store).
//! 
//! DELIBERATE directive-1 relaxation (human-approved, surfaced in the gate body + the PR): with
//! the resolver the per-PR/local loop no longer builds the toolchain from source; the DAILY
//! full suite (ci/daily-full-suite.sh, fresh main) is the SOLE remaining from-seed authoritative
//! build AND the publisher of the signed substitute. Trust = ed25519 signature (pinned key) +
//! the input-addressed NAME, NOT repro-equality (the toolchain is not byte-reproducible — task 3).
//! Durable: DEFAULT-FETCH (a path obtained without building it, runs), FALL-BACK (cold store ->
//! from-seed), self-discrimination (wrong pinned key -> reject), structural (the pinned anchor is
//! well-formed). A BUILD_GATE like td-subst: builds td-subst from source, ordered after the
//! build-recipes phase.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "toolchain-subst-default",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        // Typed artifact inputs (#353, first cutover): the runner resolves these
        // and exports TD_GATE_INPUT_{COREUTILS,BASH_STATIC}; the body's own
        // `grep LOCK | head -1` / `store-closure-scan | grep` wiring is deleted.
        inputs: &[
            // the hermetic PATH for the env -i legs (was: grep -- '-coreutils-'
            // tests/td-subst.lock | head -1)
            ArtifactInput {
                name: "coreutils",
                kind: InputKind::LockEntry { lock: "tests/td-subst.lock", stem: "coreutils" },
            },
            // the runnable no-interp fixture interned at the lock-keyed path
            // (was: grep -- '-bash-' | grep -v static on hello-no-guix.lock →
            // store-closure-scan → grep -- '-bash-static-' | head -1)
            ArtifactInput {
                name: "bash-static",
                kind: InputKind::ClosureMember {
                    lock: "tests/hello-no-guix.lock",
                    root_stem: "bash",
                    member_stem: "bash-static",
                },
            },
        ],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> toolchain-subst-default: the loop FETCHES the lock-keyed /td/store toolchain by DEFAULT (resolve-toolchain.sh) — sig+StorePath+NarHash verified, runs the fetched-not-built artifact, FALLS BACK to from-seed on a cold store / wrong key (deliberate directive-1 relaxation: the daily suite is the sole from-seed authoritative build + publisher)"
sh tests/toolchain-subst-default.sh
"##,
    }
}
