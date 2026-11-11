#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use std::path::Path;

#[test]
fn pure_std_only_source_and_standalone_gate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(!root.join("build.rs").exists());
    let files: Vec<_> = std::fs::read_dir(root.join("src"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(files, ["lib.rs"]);
    let source = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
    assert!(source.starts_with("#![forbid(unsafe_code)]"));
    // Test-only budget instrumentation is in a local cfg(test) module.
    let production = source
        .split("#[cfg(test)]")
        .next()
        .unwrap()
        .replace("#![forbid(unsafe_code)]", "");
    for forbidden in [
        "unsafe",
        "include!",
        "include_bytes!",
        "include_str!",
        "cfg_attr",
        "std::fs",
        "std::env",
        "std::net",
        "std::process",
        "std::thread",
        "std::time",
        ".unwrap()",
        ".expect(",
        "panic!",
        "unreachable!",
        "todo!",
        "unimplemented!",
    ] {
        assert!(!production.contains(forbidden), "source names {forbidden}");
    }
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(manifest.contains("td-alloc = { path = \"../td-alloc\" }"));
    assert!(manifest.contains("[workspace]\n"));
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
        assert!(manifest.contains(&format!("{lint} = \"deny\"")));
    }
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    assert_eq!(lock.matches("[[package]]").count(), 2);
    assert!(lock.contains("name = \"td-gif\""));
}

#[test]
fn budgets_are_the_documented_values() {
    assert_eq!(td_gif::MAX_AXIS, 16384);
    assert_eq!(td_gif::MAX_IMAGE_PIXELS, 16 << 20);
    assert_eq!(td_gif::MAX_INPUT_BYTES, 64 << 20);
    assert_eq!(td_gif::MAX_BLOCKS, 1 << 20);
    assert_eq!(td_gif::MAX_CODES, 32 << 20);
}
