//! Source-level pins the compiler cannot express: which files may reach
//! the system, the vault and the compositor, and the vault-document
//! policy the notebook's pane holds every entry to.
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

const FILES: [&str; 12] = [
    "app/input.rs",
    "app/layout.rs",
    "app/mod.rs",
    "app/paint.rs",
    "app/tests.rs",
    "backend.rs",
    "frames.rs",
    "main.rs",
    "mode.rs",
    "plain.rs",
    "protocol.rs",
    "window.rs",
];

/// Files that hold notebook state or text and reach nothing but memory.
const PURE: [&str; 8] = [
    "app/input.rs",
    "app/layout.rs",
    "app/mod.rs",
    "app/paint.rs",
    "frames.rs",
    "mode.rs",
    "plain.rs",
    "protocol.rs",
];

fn walk(dir: &Path, prefix: &str, found: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            walk(&entry.path(), &format!("{prefix}{name}/"), found);
        } else {
            assert!(kind.is_file(), "{name}");
            found.insert(format!("{prefix}{name}"));
        }
    }
}

#[test]
fn the_source_inventory_is_closed() {
    let mut found = BTreeSet::new();
    walk(&root().join("src"), "", &mut found);
    assert_eq!(
        found.iter().map(String::as_str).collect::<BTreeSet<_>>(),
        FILES.into_iter().collect()
    );
    assert!(!root().join("build.rs").exists());
    assert!(source("main.rs").contains("#![forbid(unsafe_code)]"));
}

#[test]
fn pure_files_reach_no_system_vault_or_compositor() {
    for name in PURE {
        let text = source(name);
        for denied in [
            "std::fs",
            "std::process",
            "std::os",
            "std::time",
            "std::net",
            "std::env",
            "std::thread",
            "std::sync::mpsc",
            "#[path",
            "include!(",
            "include_str!(",
            "td_secret",
            "td_ui::wayland",
            "td_ui::client",
            "td_ui::pinned_face",
            "td_ui::control_socket",
            "td_ui::control_worker",
            "td_ui::driven",
            "eprintln!",
            "println!",
            "td_ui as",
            "window::run(",
        ] {
            assert!(!text.contains(denied), "{name} names {denied}");
        }
        // Of the toolkit, only the drawing, widget and editor modules,
        // none of which reaches the system.
        for module in td_ui_modules(&text) {
            assert!(
                [
                    "CELL_HEIGHT",
                    "CELL_WIDTH",
                    "chrome",
                    "confirmations",
                    "editor",
                    "editor_clipboard",
                    "editor_model",
                    "editor_search",
                    "entry_model",
                    "list_model",
                    "raster",
                    "window",
                ]
                .contains(&module.as_str()),
                "{name} names td_ui::{module}"
            );
        }
    }
}

/// Every name a source reaches through `td_ui::`, braces opened.
fn td_ui_modules(text: &str) -> Vec<String> {
    let ident = |s: &str| {
        s.chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>()
    };
    let mut names = Vec::new();
    for (at, _) in text.match_indices("td_ui::") {
        let rest = &text[at + "td_ui::".len()..];
        if let Some(group) = rest.strip_prefix('{') {
            let group = &group[..group.find('}').unwrap()];
            names.extend(group.split(',').map(|item| ident(item.trim())));
        } else {
            names.push(ident(rest));
        }
    }
    assert!(!names.iter().any(String::is_empty));
    names
}

#[test]
fn only_the_backend_holds_the_vault_and_only_the_window_the_compositor() {
    for name in FILES {
        let text = source(name);
        if !matches!(name, "backend.rs" | "main.rs") {
            assert!(!text.contains("td_secret"), "{name}");
        }
        if name != "window.rs" {
            for denied in ["td_ui::wayland", "td_ui::pinned_face", "td_ui::window::run"] {
                assert!(!text.contains(denied), "{name} names {denied}");
            }
        }
    }
    // The binary names td-secret only to dispatch its token worker first.
    let main = source("main.rs");
    assert_eq!(main.matches("td_secret").count(), 1);
    assert!(main.contains("td_secret::pass::worker(&args)"));
    let worker = main.find("pass::worker").unwrap();
    assert!(worker < main.find("mode::admit").unwrap());
    assert!(worker < main.find("window::run").unwrap());
    // The files read are the system's identity, for mode admission, and
    // the mount table, to keep frames in memory; nothing is written.
    assert_eq!(main.matches("std::fs::").count(), 1);
    assert!(main.contains("std::fs::read_to_string(path)"));
    let window = source("window.rs");
    assert_eq!(window.matches("std::fs::").count(), 2);
    assert!(window.contains("std::fs::read_to_string(\"/proc/self/mountinfo\")"));
    assert!(window.contains("std::fs::canonicalize(path)"));
    // The frames go only to the directory found memory-backed.
    assert!(window.contains("td_ui::window::run(&mut session, stream, frames, typeface)"));
    assert!(!window.contains("temp_dir"));
    for name in FILES {
        if !matches!(name, "main.rs" | "window.rs") {
            assert!(!source(name).contains("std::fs"), "{name}");
        }
    }
    // The backend reaches td-secret's standalone host and nothing else of
    // the crate.
    let backend = source("backend.rs");
    assert!(backend.contains("use td_secret::pass;"));
    assert_eq!(backend.matches("td_secret").count(), 1);
    assert_eq!(backend.matches("pass::Host::open()").count(), 1);
}

#[test]
fn every_entry_document_is_held_to_the_vault_policy() {
    let app = production("app/mod.rs");
    let input = production("app/input.rs");
    // One loader, and it refuses filling before anything else is done:
    // its first dispatch after the load is Fillable with filling off.
    assert_eq!(app.matches("Event::Load(").count(), 1);
    assert_eq!(app.matches("Event::Fillable {").count(), 1);
    let start = app.find("fn load(&mut self, body: &[u8])").unwrap();
    let loader = &app[start..];
    let loader = &loader[..loader.find("\n    }\n").unwrap()];
    let load = loader.find("Event::Load(").unwrap();
    let after = &loader[load..];
    let next = after[1..].find("dispatch(").unwrap() + 1;
    let fillable = &after[next..];
    let fillable = &fillable[..fillable.find('}').unwrap()];
    assert!(
        fillable.starts_with("dispatch(Event::Fillable {"),
        "{fillable}"
    );
    assert!(fillable.contains("enabled: false"), "{fillable}");
    assert!(!app.contains("enabled: true"));
    for name in FILES {
        if name != "app/mod.rs" {
            assert!(!production(name).contains("Event::Load("), "{name}");
        }
    }
    // Copy and cut take the selection alone.
    assert!(input.contains("Snapshot::capture_selection("));
    for name in FILES {
        let text = production(name);
        assert!(!text.contains("Snapshot::capture("), "{name}");
        assert!(!text.contains("generation_for_test"), "{name}");
        for denied in [
            "pane.dispatch(Event::Resize",
            "control_socket",
            "std::fs::write",
        ] {
            assert!(!text.contains(denied), "{name} names {denied}");
        }
    }
    // Lock forgets every document and the find query, and withdraws the
    // clipboard offer.
    let clear = app.find("fn clear(&mut self)").unwrap();
    let body = &app[clear..];
    let body = &body[..body.find("\n    }\n").unwrap()];
    for step in [
        "Event::Clear",
        "History::default()",
        "self.withdraw = true",
        "self.scrub = true",
        "self.prompt = None",
        "self.paste = None",
    ] {
        assert!(body.contains(step), "lock skips {step}");
    }
}
