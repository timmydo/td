//! Shared step builders for the bootstrap-ladder rungs (#378 slices 2+3).
//!
//! Every rung recipe (recipes/src/recipes/{mes,tcc,…}.rs) composes its typed
//! `Step` list from these helpers. Conventions:
//! - `BASE_TOOLS` are the guix-seed host tools EVERY rung declares as lock
//!   inputs (replacing the deleted ladder's `ls /gnu/store/*pkg*` scavenging
//!   and the curated-PATH farm): shell, coreutils, text tools, unpackers.
//! - `base_path()` is the PATH template for Run steps: the rung's own {tools}
//!   farm first (prior-rung compilers/tools), then the base tool packages.
//! - Compression: gnu tar auto-detects by suffix and execs gzip/bzip2/xz from
//!   PATH, so the unpack helper rides on `base_path()`.

use crate::types::Step;

/// The host-tool packages every rung declares (lock input names).
pub const BASE_TOOLS: &[&str] = &[
    "bash",
    "coreutils",
    "sed",
    "grep",
    "gawk",
    "tar",
    "gzip",
    "bzip2",
    "xz",
    "findutils",
    "diffutils",
];

/// The rung PATH template: the {tools} farm first, then the base packages.
pub fn base_path() -> String {
    let mut p = String::from("{tools}");
    for t in BASE_TOOLS {
        p.push_str(&format!(":{{in:{t}}}/bin"));
    }
    p
}

/// The declared shell (the sandbox has no /bin/sh).
pub const SH: &str = "{in:bash}/bin/bash";

/// Unpack tarball input NAME into DEST (top-level dir stripped).
pub fn unpack_into(input: &str, dest: &str) -> Vec<Step> {
    vec![
        Step::MkDir { path: dest.into() },
        Step::run(
            dest,
            &[
                "{in:tar}/bin/tar",
                "-xf",
                &format!("{{in:{input}}}"),
                "--strip-components=1",
            ],
        )
        .env("PATH", &base_path()),
    ]
}

/// Unpack tarball input NAME into DEST with the top-level dir KEPT (the gcc
/// prereqs land as gmp-X.Y.Z/ subdirs that then get version-free symlinks).
pub fn unpack_keep_top(input: &str, dest: &str) -> Vec<Step> {
    vec![
        Step::MkDir { path: dest.into() },
        Step::run(dest, &["{in:tar}/bin/tar", "-xf", &format!("{{in:{input}}}")])
            .env("PATH", &base_path()),
    ]
}

/// Apply a patch input with the td-built patch rung: `patch --force -p1 -i X`
/// in {src}, env-cleared (exactly the ladder's `env -i patch …`).
pub fn apply_patch(patch_rung: &str, patch_input: &str) -> Step {
    Step::run(
        "{src}",
        &[
            &format!("{{in:{patch_rung}}}/bin/patch"),
            "--force",
            "-p1",
            "-i",
            &format!("{{in:{patch_input}}}"),
        ],
    )
}

/// `sed -i EXPR FILE…` via the declared sed (dir {src} unless absolute).
pub fn sed_i(expr: &str, files: &[&str]) -> Step {
    let mut argv: Vec<&str> = vec!["{in:sed}/bin/sed", "-i", expr];
    argv.extend_from_slice(files);
    Step::run("{src}", &argv).env("PATH", &base_path())
}
