//! td-shell-seed — North-Star: `td shell` is FULLY guix-free. Step 1 (gate td-shell) proved
//! `td shell hello -- hello` builds td's hello with no guix PROCESS; this gate closes it: with
//! a warmed seed (TD_SEED_STORE/TD_SEED_DB) td shell builds hello from the frozen seed as its
//! ONLY store DB, so /var/guix + the live /gnu/store are out of the build too. No code change —
//! run_shell's build-recipe child inherits TD_SEED_* and uses the seed-store override (#133).
//! tests/td-shell-seed.sh warms hello's seed (tools/warm-seed.sh) and runs td shell with
//! guix/Guile scrubbed from PATH: hello builds + runs from the seed (durable behavioral), every
//! input stages FROM the seed store not /gnu/store (durable structural), and the seed-built hello
//! == the guix build (removable oracle). The user-facing command builds td's own package with NO
//! guix at all — no process, no install. Heavy (stage0 + warmed seed + a hello build) →
//! BUILD_GATES + HEAVY_GATES.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "td-shell-seed",
        pools: &[Pool::Daily],
        needs: &[],
        build_gate: true,
        specs: &[],
        // Typed artifact inputs (#353): resolved by the runner —
        // the body consumes TD_GATE_INPUT_*.
        inputs: &[
            ArtifactInput {
                name: "coreutils",
                kind: InputKind::LockEntry { lock: "tests/hello-no-guix.lock", stem: "coreutils" },
            },
            ArtifactInput {
                name: "bash",
                kind: InputKind::LockEntry { lock: "tests/hello-no-guix.lock", stem: "bash" },
            },
        ],
        store: StoreMode::Private, // cold by design (#317 audit): guix-free td shell standup from the frozen seed alone
        non_blocking: true,
        script: r##"
echo ">> td-shell-seed: td shell builds + runs td's hello entirely from the frozen seed — no guix process AND no /var/guix (the user-facing command is fully guix-free)"
sh tests/td-shell-seed.sh
"##,
    }
}
