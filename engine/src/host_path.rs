//! PATH aliases to executables in an already-admitted host tool closure.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const MAX_ALIASES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Alias {
    pub name: String,
    pub source: PathBuf,
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.+".contains(&byte))
}

fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Keep only a command's first executable match in the parent PATH.
/// Nothing outside the admitted closure is copied or made visible.
/// Discovery limits omit aliases rather than disabling a workspace.
pub fn aliases(path: Option<&OsStr>, admitted: impl Fn(&Path) -> bool) -> Vec<Alias> {
    let directories: Vec<PathBuf> = path
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .take(65)
        .collect();
    if directories.len() > 64 {
        return Vec::new();
    }
    let mut aliases = BTreeMap::new();
    for directory in &directories {
        if !directory.is_absolute() || fs::canonicalize(directory).is_ok_and(|path| admitted(&path))
        {
            continue;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.take(4096) {
            let Ok(entry) = entry else {
                continue;
            };
            let Some(name) = entry
                .file_name()
                .to_str()
                .filter(|name| valid_name(name))
                .map(str::to_owned)
            else {
                continue;
            };
            if aliases.contains_key(&name)
                || !entry.file_type().is_ok_and(|kind| kind.is_symlink())
                || directories
                    .iter()
                    .map(|directory| directory.join(&name))
                    .find(|path| executable(path))
                    .as_ref()
                    != Some(&entry.path())
            {
                continue;
            }
            let Ok(source) = fs::canonicalize(entry.path()) else {
                continue;
            };
            if admitted(&source) {
                aliases.insert(name, source);
                if aliases.len() > MAX_ALIASES {
                    let _ = aliases.pop_last();
                }
            }
        }
    }
    aliases
        .into_iter()
        .map(|(name, source)| Alias { name, source })
        .collect()
}
