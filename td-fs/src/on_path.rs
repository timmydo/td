//! The executable a bare command name resolves to on `PATH`, so a test or
//! gate takes a host tool from the caller's `PATH` rather than from a
//! variable naming it.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The first regular file named `name` with an execute bit in the
/// directories of this process's `PATH`, or None. Any execute bit counts,
/// not only one this process may use.
pub fn executable_on_path(name: &str) -> Option<PathBuf> {
    executable_in(std::env::var_os("PATH").as_deref(), name)
}

/// `executable_on_path` over a given `PATH` value. Relative and empty
/// entries are skipped, so the answer never depends on the working
/// directory, and `name` must be a bare file name.
pub fn executable_in(path: Option<&OsStr>, name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return None;
    }
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::TempDir;
    use std::ffi::OsString;
    use std::fs;

    fn tool(dir: &Path, name: &str, mode: u32) {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn joined(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    #[test]
    fn the_first_executable_match_wins_and_non_executables_are_passed_over() {
        let owned = TempDir::new("td-fs-on-path").unwrap();
        let (a, b) = (owned.path().join("a"), owned.path().join("b"));
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        tool(&a, "pg", 0o644);
        tool(&b, "pg", 0o755);
        fs::create_dir(a.join("dir-tool")).unwrap();
        tool(&b, "dir-tool", 0o755);
        let path = joined(&[&a, &b]);
        assert_eq!(executable_in(Some(&path), "pg"), Some(b.join("pg")));
        assert_eq!(
            executable_in(Some(&path), "dir-tool"),
            Some(b.join("dir-tool"))
        );
        tool(&a, "pg", 0o755);
        assert_eq!(executable_in(Some(&path), "pg"), Some(a.join("pg")));
        assert_eq!(executable_in(Some(&path), "absent"), None);
    }

    #[test]
    fn relative_entries_paths_and_a_missing_path_find_nothing() {
        let owned = TempDir::new("td-fs-on-path-relative").unwrap();
        tool(owned.path(), "tool", 0o755);
        // A relative entry that does reach the tool from this working directory.
        let up = "../".repeat(std::env::current_dir().unwrap().components().count());
        let reaching = format!("{up}{}", owned.path().display());
        assert!(Path::new(&reaching).join("tool").is_file());
        let relative = OsString::from(format!(":.:relative/bin:{reaching}"));
        assert_eq!(executable_in(Some(&relative), "tool"), None);
        let path = joined(&[owned.path()]);
        for name in ["", ".", "..", "sub/tool", "/bin/sh"] {
            assert_eq!(executable_in(Some(&path), name), None, "{name}");
        }
        assert_eq!(executable_in(None, "tool"), None);
        assert_eq!(
            executable_in(Some(&path), "tool"),
            Some(owned.path().join("tool"))
        );
    }
}
