#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

//! What the compiler cannot say about td-png: two source files of its
//! own that open nothing, the engine's CRC and inflater and td-util's
//! DEFLATE encoder mounted as source and nothing else, a manifest of no dependencies, the bounds DESIGN.md
//! names, and the fixtures and hashes the decoder is held to.

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

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn two_source_files_that_open_nothing_and_mount_the_engines_two_and_deflate() {
    assert!(!root().join("build.rs").exists(), "no build script");
    assert_eq!(names("src"), set(&["encode.rs", "lib.rs"]));
    let lib = read("src/lib.rs");
    assert!(lib.starts_with("#![forbid(unsafe_code)]"));
    // Exactly the two engine files and td-util's DEFLATE encoder, each by
    // its relative path.
    let mounts: Vec<&str> = lib
        .lines()
        .filter(|l| l.trim_start().starts_with("#[path"))
        .collect();
    assert_eq!(
        mounts,
        [
            "#[path = \"../../engine/src/crc32.rs\"]",
            "#[path = \"../../engine/src/gzip.rs\"]",
            "#[path = \"../../td-util/src/deflate.rs\"]",
        ]
    );
    assert!(lib.contains("mod crc32;") && lib.contains("mod gzip;"));
    assert!(lib.contains("mod deflate;") && lib.contains("mod encode;"));
    // The encoder reaches only the root's DEFLATE encoder and CRC, so a
    // consumer can mount it beside those two without the decoder.
    let encode = read("src/encode.rs");
    assert!(!encode.contains("#[path"));
    let reached: BTreeSet<&str> = encode
        .split("crate::")
        .skip(1)
        .filter_map(|rest| rest.split("::").next())
        .collect();
    assert_eq!(reached, ["crc32", "deflate"].into_iter().collect());
    let shipped = encode.split("#[cfg(test)]").next().unwrap();
    assert!(
        !shipped.contains("super::"),
        "encode.rs reaches past the root"
    );
    // The included gzip.rs has a file entry point (`decompress_file`) that
    // td-png never calls; td-png's own text names no I/O.
    let text = lib.replace("#![forbid(unsafe_code)]", "") + &encode;
    for word in [
        "unsafe",
        "include!",
        "include_bytes!",
        "include_str!",
        "std::fs",
        "std::env",
        "std::net",
        "std::process",
        "std::thread",
        "std::time",
        "File::",
        "decompress_file",
    ] {
        assert!(!text.contains(word), "td-png's source names {word}");
    }
    assert!(text.contains("gzip::decompress_raw_prefix("));
    assert_eq!(
        names("tests"),
        set(&["confinement.rs", "decode.rs", "encode.rs", "fixtures"])
    );
    assert_eq!(
        names("tests/fixtures"),
        set(&[
            "README.md",
            "png_ref.py",
            "shapes.png",
            "shapes.svg",
            "suite"
        ])
    );
    assert_eq!(names("tests/fixtures/suite").len(), 39);
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
    assert!(lock.contains("name = \"td-png\""));
}

#[test]
fn the_bounds_are_the_ones_the_design_names() {
    assert_eq!(td_png::MAX_AXIS, 1 << 16);
    assert_eq!(td_png::MAX_IMAGE_PIXELS, 64 << 20);
    assert_eq!(td_png::MAX_INFLATED_BYTES, 256 << 20);
    assert_eq!(td_png::MAX_CHUNKS, 1 << 16);
}

#[test]
fn the_fixtures_and_the_oracle_are_the_documented_ones() {
    let shapes = std::fs::read(root().join("tests/fixtures/shapes.png")).unwrap();
    assert_eq!(shapes.len(), 4846);
    assert!(shapes.starts_with(b"\x89PNG\r\n\x1a\n"));
    let readme = read("tests/fixtures/README.md");
    assert!(readme.contains("0xaf1c86b7bf39b47b"));
    let decode = read("tests/decode.rs");
    // Every hash the decode test holds the suite to is the README's.
    let mut held = 0;
    for line in decode.lines() {
        let Some(at) = line.find("0x") else { continue };
        let hash: String = line[at..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit() || *c == 'x')
            .collect();
        if hash.len() == 18 && line.trim_start().starts_with("(\"") {
            assert!(readme.contains(&hash), "README lacks {hash}");
            held += 1;
        }
    }
    assert_eq!(held, 39);
    // The oracle's expansion contract is stated where the code lives.
    let lib = read("src/lib.rs");
    assert!(lib.contains("* 255 + 32767) / 65535"));
    assert!(lib.contains("* 255 / most"));
}
