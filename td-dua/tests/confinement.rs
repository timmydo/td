//! Source-level pins the compiler cannot express: which files may reach
//! the file system, and that only the deletion module removes anything.
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::BTreeSet;
use std::path::Path;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn source(name: &str) -> String {
    std::fs::read_to_string(root().join("src").join(name)).unwrap()
}

/// The text before a file's tail test module, as production compiles it.
fn production(name: &str) -> String {
    let text = source(name);
    text.split("#[cfg(test)]").next().unwrap().to_owned()
}

const FILES: [&str; 10] = [
    "app.rs",
    "delete.rs",
    "lib.rs",
    "main.rs",
    "scan.rs",
    "tree.rs",
    "treemap.rs",
    "view.rs",
    "window.rs",
    "worker.rs",
];

/// Files that hold the tree, the list, the treemap and the window's state
/// and reach nothing but memory.
const PURE: [&str; 4] = ["app.rs", "tree.rs", "treemap.rs", "view.rs"];

#[test]
fn the_source_inventory_is_closed() {
    let found: BTreeSet<String> = std::fs::read_dir(root().join("src"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(
        found.iter().map(String::as_str).collect::<BTreeSet<_>>(),
        FILES.into_iter().collect()
    );
    assert!(!root().join("build.rs").exists());
    for name in ["lib.rs", "main.rs"] {
        assert!(source(name).contains("#![forbid(unsafe_code)]"), "{name}");
    }
}

#[test]
fn pure_files_reach_no_system() {
    for name in PURE {
        let text = production(name);
        for denied in [
            // A grouped import would hide the paths below from this scan.
            "std::{",
            "std::fs",
            "std::process",
            "std::os::unix::fs",
            "std::net",
            "std::env",
            "std::thread",
            "std::sync::mpsc",
            "#[path",
            "include!(",
            "td_ui::wayland",
            "td_ui::client",
            "td_ui::pinned_face",
            "eprintln!",
            "println!",
            ".exists(",
            ".is_dir(",
            ".is_file(",
            ".metadata(",
            ".read_dir(",
            ".canonicalize(",
        ] {
            assert!(!text.contains(denied), "{name} names {denied}");
        }
    }
}

#[test]
fn only_the_deletion_module_removes() {
    for name in FILES {
        let text = production(name);
        for removal in [
            "remove_file",
            "remove_dir",
            "rename(",
            "set_permissions",
            "OpenOptions",
            "File::create",
            "fs::write",
        ] {
            let allowed = name == "delete.rs" && removal.starts_with("remove_");
            assert!(allowed || !text.contains(removal), "{name} names {removal}");
        }
    }
    // A directory is removed by the bounded walk, never by std's own.
    assert!(!production("delete.rs").contains("remove_dir_all"));
    assert!(production("delete.rs").contains("/proc/self/mountinfo"));
}

#[test]
fn the_manifest_names_only_the_toolkit() {
    let manifest = std::fs::read_to_string(root().join("Cargo.toml")).unwrap();
    let deps: Vec<&str> = manifest
        .split("[dependencies]")
        .nth(1)
        .unwrap()
        .lines()
        .take_while(|line| !line.starts_with('['))
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .collect();
    assert_eq!(deps, ["td-ui = { path = \"../td-ui\" }"]);
}
