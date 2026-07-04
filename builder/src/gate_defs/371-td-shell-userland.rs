//! td-shell-userland — the REAL `td shell` product command over the REAL shipped Rust userland,
//! built by td's OWN NATIVE /td/store toolchain (#258 workstream, piece D — the PRODUCT-COMMAND
//! cutover). `td shell ripgrep -- rg PATTERN tree` (and a multi-tool `td shell ripgrep fd -- …`)
//! resolves each PKG to a td RECIPE, provisions its crate closure GUIX-FREE (intern the warmed
//! source + crate set → build-recipe's TD_VENDOR_DIR form), and builds it LINKED BY the native
//! x86_64 gcc/binutils/glibc + relinked rust at /td/store — NOT the guix rust/gcc-toolchain: run_shell
//! retargets the seed lock onto the native toolchain (TD_SHELL_NATIVE_*, pre-provisioned here via
//! gate 416's assembly) and drops the guix rust/gcc-toolchain; that guix path is RETIRED for the
//! product command (a vendored rust build with no native toolchain is a hard error, never a
//! guix-rust fallback). td composes the command's PATH from the td store OUTPUT and execs the tools
//! on the host PATH (their interp/RUNPATH are /td/store, exposed via a symlink on the sandbox's
//! writable tmpfs root) — with guix/Guile SCRUBBED from PATH, so a green run proves no `guix`
//! process is in the resolve/build/exec path. This is the use-case complement to the per-tool
//! `rust-<x>` corpus gates (347/346, which keep the bespoke `crate-free-build.sh` harness on the
//! guix seed — a separate capability). All legs DURABLE behavioral (NO guix oracle): rg greps a
//! needle (not the unrelated file); the rg/fd on PATH are td's OWN builds at td store paths,
//! native-linked (zero /gnu/store bytes, interp = /td/store); fd+rg cooperate in one shell on a real
//! task; an unknown package errors with no guix fallback. The coreutils/bash/tar/gzip build seed
//! stays guix-built (retired last by the source bootstrap). HEAVY (reuses gate 416's ~45-min native
//! gcc build); the ripgrep+fd crate closures are warmed by the check.sh prelude (`td-feed warm
//! crate`, sha256 == the crates.io index cksum). Build gate → BUILD_GATES + HEAVY_GATES.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "td-shell-userland",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        // Typed artifact inputs (#353): resolved by the runner —
        // the body consumes TD_GATE_INPUT_*.
        inputs: &[
            ArtifactInput {
                name: "coreutils",
                kind: InputKind::LockEntry { lock: "tests/ripgrep.lock", stem: "coreutils" },
            },
            ArtifactInput {
                name: "bash",
                kind: InputKind::LockEntry { lock: "tests/ripgrep.lock", stem: "bash" },
            },
            // the rust-substitute-MISS path sources tests/rust-x86_64-runtime-
            // store-native.sh (assemble-only), which consumes the bash-static
            // fixture — declare it so that path resolves (#353 review find).
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
echo ">> td-shell-userland: td shell builds + runs the real Rust userland (ripgrep, fd) with td's OWN NATIVE /td/store toolchain (guix rust/gcc-toolchain retired for td shell), execs a real task (durable behavioral; no guix process, no oracle)"
sh tests/td-shell-userland.sh
"##,
    }
}
