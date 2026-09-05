//! Where the repository is, for a verb that reads it from wherever it is run:
//! the routing, `ready`, the run records and the gate roster. Engine-side so
//! the run record every invocation writes names no host module (see
//! `engine_set`).

use std::path::PathBuf;
use std::process::Command;

/// The repo root, the way the shell roots itself (`cd "$(dirname "$0")/.."`):
/// `git rev-parse --show-toplevel` when git is present, else CWD. Keeps the
/// subcommand CWD-robust like the oracle; outside a git repo it falls back to CWD.
pub(crate) fn resolve_root() -> PathBuf {
    if let Ok(o) = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        if o.status.success() {
            let top = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !top.is_empty() {
                return PathBuf::from(top);
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}
