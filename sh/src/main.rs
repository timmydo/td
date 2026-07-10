//! td-sh — td's seed shell: the brush bash-compatible shell (pure Rust, MIT)
//! wrapped as a td binary, so the bootstrap ladder's rungs can declare their
//! shell as a td seed tool instead of taking a /gnu/store bash from the host
//! (re #469: seed/recipe-only execution provenance).
//!
//! The wrapper is deliberately thin: brush-shell's `entry::run()` IS the
//! bash-compatible CLI (`-c`, script file + args, `-e`/`-x`/`-u`, `-s`,
//! `--sh`, `+O`, …), and every flag-parsing corner case it covers is
//! compatibility td would otherwise re-implement and maintain. Policy — which
//! rungs use td-sh, what goes on PATH — lives in the recipes, not here.
//!
//! The one policy this wrapper does own: `sh` alias semantics. brush parses
//! `std::env::args()` itself and does NO argv[0] detection — a `bin/sh`
//! symlink to the bare entry would silently keep full bash semantics (unlike
//! bash-as-sh, which switches to POSIX mode). So when argv[0] names us `sh`
//! (or login `-sh`), re-exec ourselves with `--sh` injected, brush's "as if
//! run as /bin/sh" mode. Verified by the sh_alias_* integration tests.

use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;

fn main() {
    if let Some(code) = reexec_sh_alias() {
        std::process::exit(code);
    }
    brush_shell::entry::run();
}

/// When invoked as `sh`/`-sh`, replace the process with `<self> --sh <args…>`.
/// Returns `None` on the normal (non-aliased, or already `--sh`-flagged) path
/// — the caller falls through to brush's CLI entry; the alias path only
/// returns on exec failure, with the exit code to die with (never silently
/// fall back to bash mode — that is the trap this fn exists to close).
fn reexec_sh_alias() -> Option<i32> {
    let mut args = std::env::args_os();
    let argv0 = args.next()?;
    let name = std::path::Path::new(&argv0).file_name()?;
    if name != "sh" && name != "-sh" {
        return None;
    }
    let rest: Vec<OsString> = args.collect();
    // Already flagged (the re-exec below, or the caller's own --sh): fall
    // through. This also terminates the re-exec recursion when the binary
    // FILE itself is named `sh` (current_exe then still ends in `sh`).
    if rest.first().is_some_and(|a| a == "--sh") {
        return None;
    }
    // /proc/self/exe, not argv[0]: inside the build sandbox the alias may be
    // a farm symlink; the target is the real binary.
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("td-sh: sh alias: cannot resolve own executable: {e}");
            return Some(127);
        }
    };
    let err = std::process::Command::new(exe).arg("--sh").args(rest).exec();
    eprintln!("td-sh: sh alias: re-exec failed: {err}");
    Some(127)
}
