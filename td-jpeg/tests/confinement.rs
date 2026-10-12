#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

//! What the compiler cannot say about td-jpeg: one pure source file that
//! opens nothing, a manifest of no dependencies, the bounds DESIGN.md
//! names, and the fixture and oracle the decoder is held to.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(path: &str) -> String {
    std::fs::read_to_string(root().join(path)).unwrap()
}

fn names(dir: &str) -> BTreeSet<String> {
    std::fs::read_dir(root().join(dir))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn one_pure_source_file_that_opens_nothing() {
    assert!(!root().join("build.rs").exists(), "no build script");
    assert_eq!(names("src"), BTreeSet::from(["lib.rs".to_string()]));
    let lib = read("src/lib.rs");
    assert!(lib.starts_with("#![forbid(unsafe_code)]"));
    let text = lib.replace("#![forbid(unsafe_code)]", "");
    for word in [
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
        "File::",
    ] {
        assert!(!text.contains(word), "lib.rs names {word}");
    }
    let tests: BTreeSet<String> = [
        "buffer.rs",
        "confinement.rs",
        "decode.rs",
        "encode.rs",
        "fixtures",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(names("tests"), tests);
    let fixtures: BTreeSet<String> = ["README.md", "jpeg_ref.py", "z8-thumb.jpg"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(names("tests/fixtures"), fixtures);
}

#[test]
fn the_manifest_has_no_dependencies_and_joins_the_gate() {
    let manifest = read("Cargo.toml");
    assert!(manifest.contains("[workspace]\n"), "own workspace root");
    assert!(!manifest.contains("dependencies]"), "no dependencies");
    assert!(!manifest.contains("path ="));
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
    assert_eq!(lock.matches("[[package]]").count(), 1);
    assert!(lock.contains("name = \"td-jpeg\""));
}

#[test]
fn the_bounds_are_the_ones_the_design_names() {
    assert_eq!(td_jpeg::MAX_AXIS, 16384);
    assert_eq!(td_jpeg::MAX_IMAGE_PIXELS, 64 << 20);
    assert_eq!(td_jpeg::MAX_PREVIEW_SAMPLES, 128 << 20);
    assert_eq!(td_jpeg::MAX_TABLE_DEFINITIONS, 32);
}

#[test]
fn the_fixture_and_the_oracle_are_the_documented_ones() {
    let thumb = std::fs::read(root().join("tests/fixtures/z8-thumb.jpg")).unwrap();
    assert_eq!(thumb.len(), 13063);
    assert!(thumb.starts_with(&[0xFF, 0xD8]) && thumb.ends_with(&[0xFF, 0xD9]));
    let readme = read("tests/fixtures/README.md");
    for hash in [
        "0x4012c2335efb6bfa",
        "0x15cf1df123ee575d",
        "0x164a0dbefb89395d",
        "0x7c9a6f6b601504ab",
        "0x8e6c3e050993ae30",
    ] {
        assert!(readme.contains(hash), "README lacks {hash}");
    }
    // The oracle's contract is stated where the constants live.
    let lib = read("src/lib.rs");
    assert!(lib.contains("floor(v + 128 + 0.5)"));
    assert!(lib.contains("1.402") && lib.contains("0.344136") && lib.contains("0.714136"));
    assert!(lib.contains("1.772"));
}
