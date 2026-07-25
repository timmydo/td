//! `which` — resolve a command name against $PATH.
//!
//! uutils' coreutils has no `which`, so this is a straight busybox replacement.

use std::path::{Path, PathBuf};

pub fn run(args: &[String]) -> Result<u8, String> {
    let mut all = false;
    let mut names: Vec<&str> = Vec::new();
    for a in args {
        match a.as_str() {
            "-a" | "--all" => all = true,
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!(
                    "unrecognised option '{other}'\nusage: which [-a] NAME..."
                ))
            }
            other => names.push(other),
        }
    }
    if names.is_empty() {
        return Err("usage: which [-a] NAME...".to_string());
    }
    // `var_os`, not `var`: a non-UTF-8 $PATH is a real (if odd) environment, and
    // `var` would turn it into an EMPTY path, reporting every command as missing
    // instead of searching the directories that are there.
    let path = std::env::var_os("PATH")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut out = String::new();
    let mut status = 0u8;
    for name in &names {
        let hits = resolve(name, &path, all);
        if hits.is_empty() {
            status = 1;
        }
        for hit in hits {
            out.push_str(&hit);
            out.push('\n');
        }
    }
    crate::emit(&out)?;
    Ok(status)
}

/// POSIX: a name containing a slash is used as given, never searched for in
/// $PATH. An empty $PATH element means the current directory.
pub fn resolve(name: &str, path: &str, all: bool) -> Vec<String> {
    resolve_from(name, path, all, ".")
}

/// `cwd` is what an empty $PATH element denotes — a parameter only so a test can
/// point it at a scratch tree instead of chdir'ing the whole process.
fn resolve_from(name: &str, path: &str, all: bool, cwd: &str) -> Vec<String> {
    let mut hits = Vec::new();
    if name.contains('/') {
        if is_executable(Path::new(name)) {
            hits.push(name.to_string());
        }
        return hits;
    }
    for dir in path.split(':') {
        let mut candidate = PathBuf::from(if dir.is_empty() { cwd } else { dir });
        candidate.push(name);
        if is_executable(&candidate) {
            hits.push(candidate.to_string_lossy().into_owned());
            if !all {
                break;
            }
        }
    }
    hits
}

/// `metadata` follows symlinks, which is what which(1) wants: a symlink to an
/// executable IS executable. A directory with the x bit set is not a command.
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    // The gate lints only non-test targets, but keep `cargo clippy --tests`
    // clean for local runs too.
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    /// Build a scratch tree: `a/tool`, `b/tool` and `a/sub/tool` executable,
    /// `a/plain` present but 0644, `a/sub` a directory.
    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("td-util-which-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["a", "b", "a/sub"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for f in ["a/tool", "b/tool", "a/plain", "a/sub/tool"] {
            let p = root.join(f);
            let mut fh = std::fs::File::create(&p).unwrap();
            fh.write_all(b"#!/bin/sh\n").unwrap();
            let mode = if f == "a/plain" { 0o644 } else { 0o755 };
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        root
    }

    #[test]
    fn resolves_the_first_match_and_all_matches() {
        let root = scratch("first");
        let path = format!("{}:{}", root.join("a").display(), root.join("b").display());

        let first = resolve("tool", &path, false);
        assert_eq!(first.len(), 1);
        assert!(first.join("").ends_with("a/tool"));

        let every = resolve("tool", &path, true);
        assert_eq!(every.len(), 2, "-a must list every match on PATH");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn non_executables_and_directories_are_not_commands() {
        let root = scratch("modes");
        let path = root.join("a").display().to_string();
        assert!(resolve("plain", &path, false).is_empty(), "0644 file is not a command");
        assert!(resolve("sub", &path, false).is_empty(), "a directory is not a command");
        assert!(resolve("absent", &path, false).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A slashed name is used AS GIVEN. The discriminating case is a relative
    /// slashed name that would resolve if it were joined onto a $PATH element:
    /// `a/sub/tool` exists and is executable, so an implementation that skipped
    /// the slash rule would report a hit for `sub/tool`. It must not.
    #[test]
    fn a_name_with_a_slash_bypasses_the_path_search() {
        let root = scratch("slash");
        let dir_a = root.join("a").display().to_string();
        assert!(
            resolve("sub/tool", &dir_a, false).is_empty(),
            "a slashed name must be used as given, not joined onto each $PATH element"
        );
        // ...and it still resolves on its own merits, with no $PATH at all.
        let direct = root.join("a/tool").display().to_string();
        assert_eq!(resolve(&direct, "", false), vec![direct.clone()]);
        let plain = root.join("a/plain").display().to_string();
        assert!(resolve(&plain, &dir_a, false).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An empty $PATH element means the current directory — the behaviour, not
    /// just the absence of a false hit. `resolve_from` stands in for a chdir so
    /// this stays safe under the test harness's threads.
    #[test]
    fn an_empty_path_element_means_the_current_directory() {
        let root = scratch("cwd");
        let cwd = root.join("a").display().to_string();
        for path in ["", ":", "/nonexistent:"] {
            let hits = resolve_from("tool", path, false, &cwd);
            assert_eq!(
                hits.len(),
                1,
                "an empty element in {path:?} must search the current directory"
            );
            assert!(hits.join("").ends_with("a/tool"));
        }
        // A non-empty element is NOT the current directory.
        assert!(resolve_from("tool", "/nonexistent", false, &cwd).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
