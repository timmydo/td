//! td-shell — `td-builder shell` is td's own `guix shell`, with NO guix: `td shell
//! PKG -- CMD` resolves PKG to a td RECIPE and BUILDS it with td-builder itself
//! (build-recipe, content-addressed cache → build-on-demand + cached), composes the
//! command's PATH from the td store OUTPUT, and execs. No `guix` process in the
//! resolve/build/exec path; an unknown package errors, it does NOT fall back to guix.
//! This is North-Star step 1 (CLAUDE.md): td shell runs guix-free; the package it
//! runs is td's OWN build at td's OWN store path (distinct from guix's). The build
//! still links the pinned toolchain SEED from the lock (guix-built today, the frozen
//! seed tarball next — step 2). tests/td-shell.sh runs `td shell` with guix/Guile
//! SCRUBBED FROM PATH (proving no guix process) and asserts: behavioral (hello
//! greets), td-built (a real /gnu/store td hello under the cache, NOT guix's path),
//! load-bearing (unknown package -> error, no guix fallback); a REMOVABLE guix
//! differential (distinct store path, same greeting). Build gate (stage0 + td-recipe-eval
//! via the build-recipes prelude) → BUILD_GATES + HEAVY_GATES.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "td-shell",
        pools: &[Pool::Heavy],
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
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> td-shell: td-builder shell builds a td package (no guix) and runs a command with it on PATH (North-Star step 1; durable behavioral + td-built + load-bearing, removable guix differential)"
sh tests/td-shell.sh
"##,
    }
}
