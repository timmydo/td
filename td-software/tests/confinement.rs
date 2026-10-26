//! Source-level pins the compiler cannot express: td-software reads two
//! image tables and talks to the compositor's control socket, and does
//! nothing else outside its window.
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
    "lib.rs",
    "main.rs",
    "registry.rs",
    "snapshot.rs",
    "window.rs",
    "worker.rs",
];

/// The window's state, the tables' grammar and the answers' parser:
/// memory alone.
const PURE: &[&str] = &["app.rs", "registry.rs", "snapshot.rs"];

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
    // The one shared source it compiles in is td-app's status record.
    for &name in FILES {
        let text = source(name);
        let expected = usize::from(name == "lib.rs");
        assert_eq!(text.matches("#[path").count(), expected, "{name}");
        assert!(!text.contains("include!("), "{name}");
    }
    assert!(source("lib.rs").contains("#[path = \"../../engine/src/flathub_app.rs\"]"));
}

#[test]
fn pure_files_reach_no_system() {
    for &name in PURE {
        let text = production(name);
        for denied in [
            // A grouped import would hide the paths below from this scan.
            "std::{",
            "std::fs",
            "std::io",
            "std::process",
            "std::os",
            "std::net",
            "std::env",
            "std::thread",
            "std::sync",
            "td_fs",
            "td_ui::wayland",
            "td_ui::client",
            "td_ui::pinned_face",
            "td_ui::control",
            "worker::run",
            "Worker",
            "eprintln!",
            "println!",
        ] {
            assert!(!text.contains(denied), "{name} names {denied}");
        }
    }
}

#[test]
fn it_starts_no_program_and_writes_no_file() {
    for &name in FILES {
        let text = production(name);
        for denied in [
            "std::process::Command",
            "Command::new",
            "exec(",
            "remove_file",
            "remove_dir",
            "rename(",
            "set_permissions",
            "OpenOptions",
            "File::create",
            "fs::write",
            "create_dir",
            "symlink",
            "hard_link",
            "fs::copy",
            "TcpStream",
            "TcpListener",
            "UdpSocket",
            "UnixListener",
            "UnixDatagram",
        ] {
            assert!(!text.contains(denied), "{name} names {denied}");
        }
        // The one spawn is the worker's thread.
        let spawns = usize::from(name == "worker.rs");
        assert_eq!(text.matches(".spawn(").count(), spawns, "{name}");
    }
    assert!(production("worker.rs").contains("std::thread::Builder::new()"));
}

#[test]
fn only_the_worker_talks_to_the_compositor_and_only_the_window_reads_files() {
    for &name in FILES {
        let text = production(name);
        let worker = name == "worker.rs";
        assert_eq!(text.contains("UnixStream"), worker, "{name}");
        assert_eq!(
            text.contains("::connect("),
            worker || name == "window.rs",
            "{name}"
        );
        let window = name == "window.rs";
        assert_eq!(text.contains("td_fs::"), window, "{name}");
        for denied in ["File::open", "read_to_string", "fs::read(", "read_dir"] {
            assert!(!text.contains(denied), "{name} names {denied}");
        }
    }
    // The window reads the two image tables and nothing else, bounded,
    // and connects only to the Wayland display.
    let window = production("window.rs");
    assert_eq!(window.matches("td_fs::").count(), 1);
    assert_eq!(window.matches("td_fs::read_bounded(").count(), 1);
    assert_eq!(window.matches("table(registry::").count(), 2);
    assert!(window.contains("table(registry::REGISTRY)"));
    assert!(window.contains("table(registry::LAUNCHER_TABLE)"));
    assert_eq!(window.matches("::connect(").count(), 1);
    assert!(window.contains("td_ui::wayland::connect(endpoint)"));
    let registry = production("registry.rs");
    assert!(registry.contains("pub const REGISTRY: &str = \"/etc/td-applications.tsv\";"));
    assert!(registry.contains("pub const LAUNCHER_TABLE: &str = \"/etc/td-launcher.tsv\";"));
    // The worker connects to the one socket the session names and writes
    // only the lines `line` renders.
    let worker = production("worker.rs");
    assert_eq!(worker.matches("::connect(").count(), 1);
    assert_eq!(worker.matches("UnixStream::connect(").count(), 1);
    assert_eq!(worker.matches("ask(").count(), 2);
    assert!(worker.contains("Some(socket) => ask(socket, &line(&job)),"));
    assert_eq!(worker.matches(".write_all(").count(), 1);
    assert!(worker.contains(".write_all(line.as_bytes())"));
    assert!(window.contains("const CONTROL_SOCKET: &str = \"TD_CONTROL_SOCKET\";"));
}

/// The environment it reads: the Wayland display's three, the pinned
/// face's setting and the control socket, all in the window, and the
/// temporary directory (`TMPDIR`) td-ui keeps its window buffers in.
#[test]
fn only_the_window_reads_the_environment_and_only_these() {
    for &name in FILES {
        let text = production(name);
        let reads = text.matches("var_os(").count() + text.matches("env::var(").count();
        let expected = if name == "window.rs" { 5 } else { 0 };
        assert_eq!(reads, expected, "{name}");
        assert!(!text.contains("env::vars"), "{name}");
    }
    let window = production("window.rs");
    for &name in FILES {
        let expected = usize::from(name == "window.rs");
        assert_eq!(
            production(name).matches("temp_dir()").count(),
            expected,
            "{name}"
        );
    }
    for read in [
        "std::env::var_os(\"WAYLAND_SOCKET\")",
        "std::env::var_os(\"WAYLAND_DISPLAY\")",
        "std::env::var_os(\"XDG_RUNTIME_DIR\")",
        "std::env::var_os(CONTROL_SOCKET)",
        "std::env::var_os(td_ui::pinned_face::SETTING)",
    ] {
        assert!(window.contains(read), "{read}");
    }
}

/// Its one argument besides the diagnostics is `--select APP`, which root
/// passes for the launcher's Install row: a name in the registry grammar,
/// checked here too, that only selects an entry the image declares.
#[test]
fn the_selected_application_is_checked_and_only_selects() {
    let main = production("main.rs");
    assert_eq!(main.matches("args_os()").count(), 1);
    assert!(main.contains(".filter(|name| td_software::registry::valid_name(name))"));
    assert!(main.contains("window::run(Some(&name))"));
    let window = production("window.rs");
    assert_eq!(window.matches("select_name(").count(), 1);
    assert!(window.contains("session.app.select_name(name);"));
}
