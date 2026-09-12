#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! Source-level contracts the compiler cannot express: what the crate is
//! made of, what it mounts from elsewhere, and what its pure modules never
//! touch.

use std::collections::BTreeSet;
use std::path::Path;

fn identifier_count(text: &str, identifier: &str) -> usize {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| *word == identifier)
        .count()
}

fn compact(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

const PURE: [&str; 8] = [
    "keyboard.rs",
    "pointer.rs",
    "repeat.rs",
    "xkb.rs",
    "xkb_compat.rs",
    "xkb_keys.rs",
    "xkb_symbols.rs",
    "xkb_syntax.rs",
];

#[test]
fn source_inventory_and_shared_mounts_are_closed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(!root.join("build.rs").exists());
    let expected: BTreeSet<String> = PURE
        .iter()
        .chain(["lib.rs"].iter())
        .map(|name| name.to_string())
        .collect();
    let mut actual = BTreeSet::new();
    for entry in std::fs::read_dir(root.join("src")).unwrap() {
        let entry = entry.unwrap();
        assert!(
            entry.file_type().unwrap().is_file(),
            "no nested or symlinked source"
        );
        let name = entry.file_name().into_string().unwrap();
        actual.insert(name.clone());
        let text = std::fs::read_to_string(entry.path()).unwrap();
        let compact = compact(&text);
        assert!(!compact.contains("include!("), "generated source in {name}");
        assert!(!compact.contains("cfg_attr"), "conditional allowance in {name}");
        assert_eq!(
            text.matches("unsafe").count(),
            usize::from(name == "lib.rs"),
            "unsafe keyword in {name}"
        );
        assert_eq!(
            compact.matches("#[path=").count(),
            if name == "lib.rs" { 3 } else { 0 },
            "source paths in {name}"
        );
        if name == "lib.rs" {
            assert!(compact.starts_with("#![forbid(unsafe_code)]"));
            for (file, declaration) in [
                ("font.rs", "pubmodfont;"),
                ("font_data.rs", "modfont_data;"),
                ("wire.rs", "pubmodwire;"),
            ] {
                assert!(
                    compact.contains(&format!(
                        "#[path=\"../../td-compositor/src/{file}\"]{declaration}"
                    )),
                    "shared mount of {file}"
                );
                let shared =
                    std::fs::read_to_string(root.join("../td-compositor/src").join(file)).unwrap();
                assert!(!shared.contains("unsafe"), "unsafe through shared source: {file}");
                for interface in ["wl_seat", "wl_keyboard", "wl_pointer"] {
                    assert!(
                        !shared.contains(interface),
                        "input binding through shared source: {file}"
                    );
                }
                // The compiler modules by name, as td-editor pinned them;
                // `pointer` and `repeat` are ordinary words a codec may use.
                for module in [
                    "keyboard",
                    "xkb",
                    "xkb_syntax",
                    "xkb_keys",
                    "xkb_symbols",
                    "xkb_compat",
                ] {
                    assert_eq!(
                        identifier_count(&shared, module),
                        0,
                        "shared source reaching the input layer: {file}"
                    );
                }
                assert_eq!(
                    identifier_count(&shared, "sys"),
                    0,
                    "raw-module access through shared source: {file}"
                );
                let shared = self::compact(&shared);
                assert!(!shared.contains("#[path="));
                assert!(!shared.contains("include!("));
                assert!(!shared.contains("cfg_attr"));
            }
        }
        if PURE.contains(&name.as_str()) {
            // Production text only: a module's own `#[cfg(test)] mod tests`
            // may read a fixture, as `repeat.rs` does. That module is held to
            // being the file's tail — one block, opened right after the
            // attribute, nothing at column zero below it but its closing
            // brace — so no production item can shelter beneath it.
            let (text, tests) = text
                .split_once("#[cfg(test)]")
                .unwrap_or((text.as_str(), ""));
            assert!(!tests.contains("#[cfg(test)]"), "one test block in {name}");
            if !tests.is_empty() {
                let mut lines = tests
                    .lines()
                    .skip(1)
                    .skip_while(|line| line.starts_with("#[allow("));
                assert_eq!(lines.next(), Some("mod tests {"), "test block in {name}");
                let mut closed = false;
                for line in lines {
                    assert!(
                        line.is_empty() || (!closed && (line.starts_with(' ') || line == "}")),
                        "item below the test module in {name}: {line}"
                    );
                    closed |= line == "}";
                }
                assert!(closed, "unclosed test module in {name}");
            }
            for ambient in [
                "std::env",
                "std::fs",
                "std::net",
                "std::process",
                "std::time",
                "std::os",
                "std::io",
                "Instant",
                "SystemTime",
                "include_str!",
                "include_bytes!",
            ] {
                assert!(
                    !text.contains(ambient),
                    "ambient access `{ambient}` in pure module {name}"
                );
            }
        }
    }
    assert_eq!(actual, expected);
}

#[test]
fn the_crate_depends_on_nothing_and_declares_its_gate() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    let code: String = manifest
        .lines()
        .map(|line| line.split('#').next().unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !code.contains("dependencies"),
        "td-ui is the leaf: it names no dependency table of any kind"
    );
    assert!(code.contains("[workspace]"), "own workspace root");
    assert!(code.contains("[package.metadata.td-gate]"));
    assert!(code.contains("clippy-all-targets = true"));
    for lint in [
        "unwrap_used",
        "expect_used",
        "panic",
        "unreachable",
        "todo",
        "unimplemented",
        "indexing_slicing",
    ] {
        assert!(code.contains(&format!("{lint} = \"deny\"")), "{lint} denied");
    }
    let lock =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock")).unwrap();
    assert_eq!(
        lock.lines().filter(|line| line.trim() == "[[package]]").count(),
        1,
        "the leaf's lock lists only itself"
    );
    assert!(!lock.contains("source ="));
}
