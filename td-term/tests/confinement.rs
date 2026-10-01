#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Source-level contracts the compiler cannot express: the crate's file
//! inventory, that it forbids `unsafe` and declares the toolkit as its one
//! dependency, that the device and raw layers are td-ui's, and the words
//! td's boot oracle and units read, by value.

use std::collections::BTreeSet;
use std::path::Path;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(root().join(relative)).unwrap_or_else(|e| panic!("{relative}: {e}"))
}

fn names(dir: &str) -> BTreeSet<String> {
    std::fs::read_dir(root().join(dir))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

const SOURCES: [&str; 4] = ["app.rs", "main.rs", "ready.rs", "session.rs"];

/// A file's production text: everything before its test module.
fn production(name: &str) -> String {
    let text = read(&format!("src/{name}"));
    text.split_once("#[cfg(test)]\nmod tests {")
        .map_or(text.clone(), |(production, _)| production.to_string())
}

#[test]
fn source_inventory_is_closed() {
    assert!(!root().join("build.rs").exists(), "no build script");
    let expected: BTreeSet<String> = SOURCES.iter().map(|s| s.to_string()).collect();
    assert_eq!(names("src"), expected);
    assert_eq!(
        names("tests"),
        BTreeSet::from(["confinement.rs".to_string()])
    );
}

#[test]
fn the_crate_forbids_unsafe_and_reaches_no_raw_layer() {
    assert!(read("src/main.rs").contains("\n#![forbid(unsafe_code)]\n"));
    for name in SOURCES {
        let text = read(&format!("src/{name}")).replace("#![forbid(unsafe_code)]", "");
        assert!(!text.contains("unsafe"), "{name} names unsafe");
        assert!(!text.contains("cfg_attr"), "{name} uses cfg_attr");
        assert!(!text.contains("#[path"), "{name} mounts a path");
        let production = production(name);
        for forbidden in [
            "include!",
            "include_str!",
            "include_bytes!",
            "from_raw_fd",
            "as_raw_fd",
            "libc",
            "asm!",
        ] {
            assert!(!production.contains(forbidden), "{name} names {forbidden}");
        }
    }
    // The PTY device, its ioctls and its threads are td-ui's (UNSAFE.md
    // section 19); the terminal holds only its policy.
    let session = production("session.rs");
    assert!(!session.contains("Command::"), "spawning is td-ui's");
    assert!(session.contains("(\"TERM\".into(), \"td-term\".into()),"));
    assert!(session.contains("pub const DEFAULT_SHELL: &str = \"/bin/sh\";"));
    assert!(
        !session.contains("cttyhack\";"),
        "the shell leads its session itself"
    );
}

#[test]
fn the_words_units_and_the_boot_oracle_read_are_pinned() {
    let app = production("app.rs");
    for pin in [
        "pub const TITLE: &str = \"td terminal\";",
        "const APP_ID: &str = \"td-term\";",
        "const CLIPBOARD_PROOF_CMDLINE_TOKEN: &[u8] = b\"td.firefox-input=1\";",
        "const CLIPBOARD_TARGET_PREFIX: &str = \"TD-TERM-CLIPBOARD-TARGET-READY\";",
        "const CLIPBOARD_FOCUS_PREFIX: &str = \"TD-TERM-CLIPBOARD-FOCUS-READY serial=\";",
        "\"TD-TERM-CLIPBOARD-SELECTION-READY bytes={}\\n\"",
        "\"TD-TERM-CLIPBOARD-READY bytes={length}\\n\"",
        "\"TD-TERM-CLIPBOARD-SENT bytes={}\\n\"",
        "const LAST_SCREEN_PREFIX: &str = \"td-term: last screen\";",
        "format!(\"the terminal's child exited with status {code}\")",
    ] {
        assert!(app.contains(pin), "{pin}");
    }
    assert!(production("ready.rs").contains("pub const MARKER: &str = \"TD-TERM-READY\";"));
    let main = production("main.rs");
    assert!(main.contains("writeln!(std::io::stderr().lock(), \"td-term: {error}\")"));
    assert!(main.contains("writeln!(out, \"TD-TERM-SELFTEST-OK\")"));
}

#[test]
fn the_manifest_declares_the_toolkit_alone_and_joins_the_gate() {
    let manifest = read("Cargo.toml");
    assert!(manifest.contains("[workspace]\n"), "own workspace root");
    assert!(
        manifest.contains("\n[dependencies]\ntd-ui = { path = \"../td-ui\" }\n"),
        "the toolkit by path"
    );
    assert_eq!(manifest.matches("path =").count(), 1, "one dependency");
    for table in [
        "[dev-dependencies]",
        "[build-dependencies]",
        "[target",
        "[patch",
        "[replace",
    ] {
        assert!(!manifest.contains(table), "{table}");
    }
    assert!(manifest.contains("[package.metadata.td-gate]\nclippy-all-targets = true\n"));
    for lint in [
        "unwrap_used",
        "expect_used",
        "panic",
        "unreachable",
        "todo",
        "unimplemented",
        "indexing_slicing",
    ] {
        assert!(manifest.contains(&format!("{lint} = \"deny\"")), "{lint}");
    }
    let lock = read("Cargo.lock");
    assert_eq!(lock.matches("[[package]]").count(), 2);
    assert!(lock.contains("name = \"td-term\""));
    assert!(lock.contains("name = \"td-ui\""));
    assert!(!lock.contains("source ="), "no registry or git source");
    assert!(
        !root().join(".cargo").exists(),
        "no crate-local cargo config"
    );
    assert_eq!(read(".gitignore"), "/target/\n");
}
