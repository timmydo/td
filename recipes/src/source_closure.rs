//! The trees a local-source recipe's build reads by name: every file its
//! crates name by a literal `#[path]` or `include` macro, test code
//! included, and every file those name in turn. Recipe tests hold a
//! recipe's staged trees to it.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// Every literal `#[path]`, `include!`, `include_str!` and
/// `include_bytes!` in `text`, with any `cfg_attr` path, as the file
/// spells it; a name built with `concat!` is not one.
pub(crate) fn named(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    for marker in [
        "path = \"",
        "include!(\"",
        "include_str!(\"",
        "include_bytes!(\"",
    ] {
        for (at, _) in text.match_indices(marker) {
            let rest = text.get(at + marker.len()..).unwrap_or_default();
            if let Some(end) = rest.find('"') {
                found.push(rest.get(..end).unwrap_or_default());
            }
        }
    }
    found
}

/// `path` with its `..` and `.` components taken out lexically.
pub(crate) fn normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                assert!(out.pop(), "{} leaves the checkout", path.display());
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The checkout's top trees that the `.rs` files under each of `crates`'
/// `src` name, transitively, and how many names were followed. Each name
/// must be a file.
pub(crate) fn trees_named(crates: &[&str]) -> (BTreeSet<String>, usize) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let mut pending: Vec<PathBuf> = Vec::new();
    for krate in crates {
        let mut dirs = vec![root.join(krate).join("src")];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    pending.push(path.strip_prefix(&root).unwrap().to_path_buf());
                }
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut trees = BTreeSet::new();
    let mut mounts = 0;
    while let Some(file) = pending.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        let text = std::fs::read_to_string(root.join(&file)).unwrap();
        for name in named(&text) {
            let target = normal(&file.parent().unwrap().join(name));
            assert!(
                root.join(&target).is_file(),
                "{} names {name}, which is not a file",
                file.display()
            );
            let tree = target.components().next().unwrap();
            trees.insert(tree.as_os_str().to_string_lossy().into_owned());
            mounts += 1;
            if target.extension().is_some_and(|e| e == "rs") {
                pending.push(target);
            }
        }
    }
    (trees, mounts)
}
