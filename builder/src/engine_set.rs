//! The builder's ENGINE sources, for the memo key a recipe check is answered
//! from: the files a build can execute. `build.rs` takes a digest over them
//! at compile time, `td-builder engine-fingerprint` prints it, and the
//! evaluator keys a check's verdict memo and plan memo on that rather than on
//! the binary's bytes, so an edit to the host-only tooling below re-keys
//! nothing while an edit to anything a build runs re-keys everything.
//!
//! A host-only file is one the binary reaches only through a verb no check
//! invokes — `ready`, `affected-checks`, `check`, `check-rung`, `gate-run`,
//! `gate-body`, `gate-crates` — and never from an engine module: `main.rs`
//! names each only in the arm that dispatches its verb, and no other engine
//! file names one at all. `affected.rs` pins both directions, over the
//! shipped half of every file, in `engine_sources_never_name_a_host_only_module`.
//! A helper both sides need lives on the engine side and the host imports it:
//! the repo root in `repo.rs`, the daemon dir in `build_daemon.rs`, the lock
//! checksums in `cargo_lock.rs`, the CPU count in `check_memory.rs`, the host
//! cargo build in `host_bin.rs`, the unprovisioned signal in the engine
//! crate's `exit`.
//!
//! What the rule does not cover, on purpose: the host toolchain that
//! compiles the builder, outside the key as it is for the evaluator's
//! source fingerprint (the repo's cargo config is in; a toolchain pin is
//! not read); the `net/` sources `provision-net` builds td-net from, since
//! that tool fetches hash-pinned inputs and decides no verdict; and the
//! gate's invocation of a check (`gate_bodies.rs`), since the spec and
//! index it passes are in the key, the builder it points at reports this
//! digest, and its job budget changes speed alone. An `impl` on an engine
//! type from a host-only file, or a macro a host-only file exports, would
//! let an engine caller run host code without naming it; the test forbids
//! both.
//!
//! A different question from `affected::HOST_ONLY_ENGINE_SOURCES`, which
//! exempts a file from the FROM-SOURCE tier a change to it selects and keeps
//! `affected.rs` in that tier because it decides what runs. The memo key asks
//! only what a check's own run can execute, and a check never runs the
//! dispatcher. `ready.rs` is in both.
//!
//! Included by `build.rs` through `#[path]` and declared for the tests in
//! `main.rs`, so the list the fingerprint skips and the list the test pins
//! are one.

/// Paths under `builder/src`; a trailing `/` names a directory.
pub const HOST_ONLY: &[&str] = &[
    "affected.rs",
    "check_loop.rs",
    "gate_bodies.rs",
    "gate_defs/",
    "gate_lint.rs",
    "gate_timing.rs",
    "gates.rs",
    "ready.rs",
];

/// The verbs whose dispatch arms may name a host-only module: the ones no
/// check invokes. The test holds the dispatch to exactly this set in both
/// directions, so an engine verb routed into a host-only module — code a
/// build would then execute outside the key — is a failing test, and so is
/// a verb listed here that reaches none.
pub const HOST_ONLY_VERBS: &[&str] = &[
    "affected-checks",
    "check",
    "check-rung",
    "gate-body",
    "gate-crates",
    "gate-run",
    "ready",
];

/// Whether `rel`, a `/`-separated path under `builder/src`, is host-only:
/// one of the files above, or anything under one of the directories.
pub fn is_host_only(rel: &str) -> bool {
    HOST_ONLY.iter().any(|entry| match entry.strip_suffix('/') {
        Some(dir) => rel
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/')),
        None => rel == *entry,
    })
}

/// Whether `rel` is one of the directories above, so a walk need not enter
/// it: the entry itself, not a file under it (`is_host_only` answers that).
pub fn is_host_only_dir(rel: &str) -> bool {
    HOST_ONLY
        .iter()
        .any(|entry| entry.strip_suffix('/') == Some(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_only_directory_is_the_entry_itself() {
        assert!(is_host_only_dir("gate_defs"));
        assert!(!is_host_only_dir("gate_defs/"));
        assert!(!is_host_only_dir("gate_defs/x"));
        assert!(!is_host_only_dir("gate_defsx"));
        assert!(!is_host_only_dir("affected.rs"), "a file is not a directory entry");
    }

    #[test]
    fn a_host_only_path_is_the_file_or_under_the_directory() {
        assert!(is_host_only("affected.rs"));
        assert!(is_host_only("ready.rs"));
        assert!(is_host_only("gate_defs/325-cargo-test.rs"));
        assert!(is_host_only("gate_defs/deep/x.rs"));
        assert!(!is_host_only("gate_defs"), "the directory entry itself is not a file");
        assert!(!is_host_only("gate_defsx/a.rs"));
        assert!(!is_host_only("affected.rs.bak"));
        assert!(!is_host_only("main.rs"));
        assert!(!is_host_only("build.rs"));
        assert!(!is_host_only("check_host.rs"));
        assert!(!is_host_only("run_record.rs"));
        assert!(!is_host_only("gate_inputs.rs"), "the store-path helper is engine code");
    }

    /// Every entry exists, so a rename cannot leave the list naming nothing
    /// and quietly fingerprint the renamed file.
    #[test]
    fn every_host_only_entry_exists() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for entry in HOST_ONLY {
            let path = src.join(entry.trim_end_matches('/'));
            match entry.strip_suffix('/') {
                Some(_) => assert!(path.is_dir(), "{entry} is not a directory"),
                None => assert!(path.is_file(), "{entry} is not a file"),
            }
        }
    }
}
