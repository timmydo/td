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

const FILES: &[&str] = &[
    "app/input.rs",
    "app/layout.rs",
    "app/mod.rs",
    "app/paint.rs",
    "app/tests.rs",
    "backend.rs",
    "backend/fixture.rs",
    "files.rs",
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
        FILES.iter().copied().collect()
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
            // A path's own methods that reach the file system.
            ".exists(",
            ".try_exists(",
            ".is_dir(",
            ".is_file(",
            ".is_symlink(",
            ".metadata(",
            ".symlink_metadata(",
            ".read_dir(",
            ".read_link(",
            ".canonicalize(",
        ] {
            assert!(!text.contains(denied), "{name} names {denied}");
        }
        // Of the toolkit, only the drawing, text, widget and editor modules,
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
                    "finder",
                    "keys",
                    "list_model",
                    "raster",
                    "text",
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
    for &name in FILES {
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
    for &name in FILES {
        if !matches!(
            name,
            "main.rs" | "window.rs" | "files.rs" | "backend/fixture.rs"
        ) {
            assert!(!source(name).contains("std::fs"), "{name}");
        }
    }
    // The backend reaches td-secret's standalone host and its host events,
    // and nothing else of the crate.
    let backend = source("backend.rs");
    assert!(backend.contains("use td_secret::pass;"));
    assert_eq!(backend.matches("td_secret").count(), 1);
    assert_eq!(backend.matches("pass::Host::open(").count(), 1);
    assert!(backend.contains("pass::Host::open(accepted)"));
    // Swap is accepted only by the window's AcceptSwap, which passes back
    // the risk this thread kept from the last open.
    assert_eq!(backend.matches("risk.take()").count(), 1);
    // and the window sends that only from Open anyway in its question.
    let sent: Vec<usize> = [
        "app/input.rs",
        "app/layout.rs",
        "app/mod.rs",
        "app/paint.rs",
    ]
    .iter()
    .map(|name| production(name).matches("Command::AcceptSwap").count())
    .collect();
    assert_eq!(sent, [1, 0, 0, 0]);
    assert!(production("app/input.rs").contains(
        "(Choice::Confirmed(Act::AcceptSwap), _) => {\n                        self.phase = Phase::Opening;\n                        self.out.push(Out::Send(Command::AcceptSwap));"
    ));
    assert!(
        backend.contains("Command::AcceptSwap => {\n                let accepted = risk.take();")
    );
    // Beside the vault, files reach folder listings and encrypted copies
    // alone: a copy is written as a new private file, never over another,
    // and read to a bound; only a partial copy it made is removed.
    let files = production("files.rs");
    // Two opens: the one write, new and private, and the read, which
    // does not wait on a FIFO.
    assert_eq!(files.matches("OpenOptions::new()").count(), 2);
    assert_eq!(files.matches(".write(true)").count(), 1);
    assert_eq!(files.matches(".create_new(true)").count(), 1);
    assert_eq!(files.matches(".mode(0o600)").count(), 1);
    assert_eq!(files.matches(".custom_flags(NONBLOCK)").count(), 1);
    assert_eq!(files.matches("remove_file(&path)").count(), 1);
    assert!(files.contains(".take(ceiling as u64 + 1)"));
    for denied in [
        "fs::write",
        "File::create",
        "fs::copy",
        "hard_link",
        "unix::fs::symlink",
        "rename(",
        "remove_dir",
        "set_permissions",
        "create_dir",
        "as fs",
        "as OpenOptions",
    ] {
        assert!(!files.contains(denied), "files.rs names {denied}");
    }
    // The vault thread writes and reads copies only through them, and the
    // window lists folders through them.
    assert_eq!(backend.matches("files::").count(), 2);
    assert!(backend.contains("crate::files::write_copy(&folder, &name, &bytes)"));
    assert!(backend.contains("crate::files::read_copy(&path, pass::MAX_COPY)"));
    let window = source("window.rs");
    assert_eq!(window.matches("files::").count(), 2);
    assert!(window.contains("crate::files::start_folder"));
    assert!(window.contains("crate::files::list_folder(&folder, ceiling)"));
    for &name in FILES {
        if !matches!(name, "backend.rs" | "window.rs" | "main.rs" | "files.rs") {
            assert!(!source(name).contains("files::"), "{name}");
        }
    }
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
    for &name in FILES {
        if name != "app/mod.rs" {
            assert!(!production(name).contains("Event::Load("), "{name}");
        }
    }
    // Copy and cut take the selection alone.
    assert!(input.contains("Snapshot::capture_selection("));
    for &name in FILES {
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

/// The test vault is built only with `test-vault`, which nothing enables
/// by default: its one mount and both of its uses are behind the feature,
/// no other file names it, and it reaches td-secret only through the
/// backend's import and the file system only in its case directory.
#[test]
fn the_test_vault_is_built_only_by_its_feature() {
    let backend = source("backend.rs");
    let gated = |item: &str| {
        assert!(
            backend.contains(&format!("#[cfg(feature = \"test-vault\")]\n{item}\n")),
            "{item}"
        );
        assert!(!backend.contains(&format!("#[cfg(not(feature = \"test-vault\"))]\n{item}\n")));
    };
    gated("mod fixture;");
    gated("use fixture::serve as vault;");
    gated("use fixture::watch;");
    assert_eq!(backend.matches("fixture").count(), 3);
    assert!(backend.contains("#[cfg(not(feature = \"test-vault\"))]\nuse serve as vault;\n"));
    assert!(backend.contains(".spawn(move || vault(&job_rx, &answer_rx, &reply_tx))"));
    assert!(backend.contains("let _ = started.send(watch());"));
    for &name in FILES {
        if !matches!(name, "backend.rs" | "backend/fixture.rs" | "main.rs") {
            assert!(!source(name).contains("fixture"), "{name}");
            assert!(!source(name).contains("test-vault"), "{name}");
        }
    }
    // The binary admits its mode from a synthetic identity in the test
    // vault's build alone.
    let main = source("main.rs");
    assert!(!main.contains("fixture"));
    assert_eq!(main.matches("test-vault").count(), 3);
    assert_eq!(main.matches("feature = \"test-vault\"").count(), 2);
    assert!(main
        .contains("#[cfg(not(feature = \"test-vault\"))]\nfn os_release() -> Option<String> {\n"));
    assert!(main.contains(
        "#[cfg(feature = \"test-vault\")]\nfn os_release() -> Option<String> {\n    Some(\"ID=td-pass-test-vault\\n\".to_owned())\n}\n"
    ));
    let manifest = std::fs::read_to_string(root().join("Cargo.toml")).unwrap();
    assert!(manifest.contains("\n[features]\ntest-vault = []\n\n"));
    assert!(!manifest.contains("default ="));
    let fixture = source("backend/fixture.rs");
    assert!(!fixture.contains("td_secret"));
    assert!(fixture.contains("use super::{closed, pass, refused, Job};"));
    assert_eq!(fixture.matches("pass::").count(), 1);
    assert_eq!(fixture.matches("std::fs::").count(), 2);
    for alias in [
        "std::fs as",
        "pass as",
        "super::serve",
        ".exists()",
        "std::fs;",
    ] {
        assert!(!fixture.contains(alias), "{alias}");
    }
    assert!(fixture.contains(".open(self.directory.join(\"journal\"))"));
    assert!(fixture.contains("std::fs::remove_file(self.directory.join(name))"));
}
