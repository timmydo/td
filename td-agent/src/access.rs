//! Explicit conversation grants, checked again whenever a workspace is prepared.
use crate::{
    config, git, jail,
    store::StateDir,
    workspace::{self, Shared},
};
use std::path::{Path, PathBuf};

pub fn admitted(state: &StateDir, requested: &[Shared]) -> Result<Vec<Shared>, String> {
    if requested.is_empty() {
        return Ok(Vec::new());
    }
    if requested.len() > 16 {
        return Err("at most 16 conversation directory grants".into());
    }
    let path = config::path(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    );
    let config = config::load(path.as_deref())?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME missing")?;
    let programs = jail::Programs::from_env()?;
    let mut places =
        workspace::Places::from_env(state.root(), &config.workspace_root(&home), &programs)?;
    let mut remotes = config.remotes;
    for remote in state.load_admitted()? {
        remotes.push(git::Admission::parse(&remote)?);
    }
    places.locals = remotes
        .iter()
        .filter_map(|admission| match admission {
            git::Admission::Exact(remote) => remote.local_path(),
            git::Admission::Prefix { .. } => None,
        })
        .collect();
    let (grants, notes) = workspace::admit_shared(requested, &places);
    if !notes.is_empty() {
        return Err(notes.join("; "));
    }
    // Readmission must not silently follow a replaced symlink to a new directory.
    if grants != requested {
        return Err("a directory grant changed its canonical path; request it again".into());
    }
    Ok(grants)
}

pub fn disjoint(grants: &[Shared], paths: &[PathBuf]) -> Result<(), String> {
    for grant in grants {
        if paths.iter().any(|path| overlap(&grant.path, path)) {
            return Err(format!(
                "directory grant {} overlaps an existing workspace mount",
                grant.path.display()
            ));
        }
    }
    Ok(())
}

fn overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Stored paths remain revocable even after their host target changes.
pub fn revoke_path(path: &Path, grants: &[Shared]) -> PathBuf {
    if grants.iter().any(|grant| grant.path == path) {
        path.to_path_buf()
    } else {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    #[test]
    fn revoke_matches_aliases_and_replaced_stored_paths() {
        let owned = td_fs::TempDir::new("revoke-path").unwrap();
        let root = std::fs::canonicalize(owned.path()).unwrap();
        let data = root.join("data");
        let other = root.join("other");
        let alias = root.join("alias");
        std::fs::create_dir(&data).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&data, &alias).unwrap();
        let grants = [Shared {
            path: data.clone(),
            write: false,
        }];
        assert_eq!(revoke_path(&alias, &grants), data);
        std::fs::remove_dir(&data).unwrap();
        std::os::unix::fs::symlink(&other, &data).unwrap();
        assert_eq!(revoke_path(&data, &grants), data);
        std::fs::remove_file(&data).unwrap();
        assert_eq!(revoke_path(&data, &grants), data);
    }
}
