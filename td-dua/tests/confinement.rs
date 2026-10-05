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

const FILES: &[&str] = &[
    "app.rs",
    "delete.rs",
    "lib.rs",
    "main.rs",
    "report.rs",
    "scan.rs",
    "tree.rs",
    "treemap.rs",
    "view.rs",
    "window.rs",
    "worker.rs",
];

/// Files that hold the tree, the list, the treemap, the window's state and
/// the cleanup report, and reach nothing but memory.
const PURE: &[&str] = &["app.rs", "report.rs", "tree.rs", "treemap.rs", "view.rs"];

#[test]
fn the_source_inventory_is_closed() {
    let found: BTreeSet<String> = std::fs::read_dir(root().join("src"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(
        found.iter().map(String::as_str).collect::<BTreeSet<_>>(),
        FILES.iter().copied().collect()
    );
    assert!(!root().join("build.rs").exists());
    for name in ["lib.rs", "main.rs"] {
        assert!(source(name).contains("#![forbid(unsafe_code)]"), "{name}");
    }
}

#[test]
fn pure_files_reach_no_system() {
    for &name in PURE {
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
    for &name in FILES {
        let text = production(name);
        for removal in [
            "remove_file",
            "remove_dir",
            "rename(",
            "set_permissions",
            "OpenOptions",
            "File::options",
            "File::create",
            "fs::write",
        ] {
            // The report's tag reader opens one file read-only, below.
            let allowed = (name == "delete.rs" && removal.starts_with("remove_"))
                || (name == "scan.rs" && removal == "OpenOptions");
            assert!(allowed || !text.contains(removal), "{name} names {removal}");
        }
    }
    let scan = production("scan.rs");
    assert_eq!(scan.matches("OpenOptions::new()").count(), 1);
    assert!(scan.contains(".read(true)"));
    // The flags are exactly these three, so none adds O_CREAT or O_TRUNC.
    assert_eq!(scan.matches(".custom_flags(").count(), 1);
    assert!(scan.contains(".custom_flags(O_NOFOLLOW | O_NONBLOCK | O_NOCTTY)"));
    for flag in [
        "const O_NONBLOCK: i32 = 0o4000;",
        "const O_NOCTTY: i32 = 0o400;",
        "const O_NOFOLLOW: i32 = 0o400000;",
    ] {
        assert_eq!(scan.matches(flag).count(), 1, "{flag}");
    }
    assert_eq!(scan.matches("const O_").count(), 3);
    for writes in [
        ".write(",
        ".append(",
        ".create(",
        ".truncate(",
        "create_new",
    ] {
        assert!(!scan.contains(writes), "scan.rs names {writes}");
    }
    // A directory is removed by the bounded walk, never by std's own.
    assert!(!production("delete.rs").contains("remove_dir_all"));
    assert!(production("delete.rs").contains("/proc/self/mountinfo"));
}

/// td-civil also reads the clock and the zone file, so a pure file may name
/// only the calendar's pure functions it uses, each by full path.
#[test]
fn pure_files_take_only_the_calendar_from_td_civil() {
    for &name in PURE {
        let text = production(name);
        for hidden in ["use td_civil", "td_civil::{", "td_civil as"] {
            assert!(!text.contains(hidden), "{name}: {hidden}");
        }
        for (at, _) in text.match_indices("td_civil::") {
            let used: String = text[at + "td_civil::".len()..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            assert!(
                ["format_ymd", "unix_to_civil_utc_checked"].contains(&used.as_str()),
                "{name} names td_civil::{used}"
            );
        }
    }
}

#[test]
fn the_manifest_names_the_calendar_the_json_writer_and_the_toolkit() {
    let manifest = std::fs::read_to_string(root().join("Cargo.toml")).unwrap();
    let deps: Vec<&str> = manifest
        .split("[dependencies]")
        .nth(1)
        .unwrap()
        .lines()
        .take_while(|line| !line.starts_with('['))
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .collect();
    assert_eq!(
        deps,
        [
            "td-civil = { path = \"../td-civil\" }",
            "td-json = { path = \"../td-json\" }",
            "td-ui = { path = \"../td-ui\" }"
        ]
    );
    let lock = std::fs::read_to_string(root().join("Cargo.lock")).unwrap();
    let names: Vec<&str> = lock
        .lines()
        .filter_map(|line| line.strip_prefix("name = \""))
        .filter_map(|rest| rest.strip_suffix('"'))
        .collect();
    assert_eq!(names, ["td-civil", "td-dua", "td-json", "td-ui"]);
    assert!(!lock.contains("source ="), "no registry or git source");
}
