#![cfg(test)]
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]

use std::path::Path;
use td_crypto::Digest;

fn words(source: &str, word: &str) -> usize {
    source
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|token| *token == word)
        .count()
}

fn fingerprint(source: &str, expected: &str) {
    use std::fmt::Write;
    let mut digest = td_crypto::Sha256::try_new().unwrap();
    digest.update(source.as_bytes()).unwrap();
    let mut hex = String::new();
    for byte in digest.finish().unwrap() {
        write!(&mut hex, "{byte:02x}").unwrap();
    }
    assert_eq!(
        hex, expected,
        "review the full confined surface before repinning"
    );
}

fn scan(root: &Path, path: &Path) {
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        assert!(!entry.file_type().unwrap().is_symlink());
        if path.is_dir() {
            scan(root, &path);
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let relative = path.strip_prefix(root).unwrap().to_str().unwrap();
        if relative == "tests/allocation_confinement.rs" {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        if relative == "tests/support/allocation_shim.rs" {
            assert_eq!(words(&source, "unsafe"), 9);
            assert_eq!(source.matches("#[allow(unsafe_code)]").count(), 1);
            assert_eq!(source.matches("#[global_allocator]").count(), 1);
            fingerprint(
                &source,
                "5e32cd6f5f5c9e32c073ef5cbbed1afe01e939fdc92c9f8e97c8b3566a09bf6b",
            );
        } else if relative == "tests/support/native_allocator_bridge.rs" {
            assert_eq!(words(&source, "unsafe"), 20);
            assert_eq!(source.matches("#[allow(unsafe_code)]").count(), 7);
            assert!(!source.contains("global_allocator"));
            fingerprint(
                &source,
                "f552000f6717c0eee3a3b3ab8fa6abea2c2f74e7fed9bf7a9dd2b1344509fcd3",
            );
        } else if relative == "tests/support/native_allocation_controls.rs" {
            assert_eq!(words(&source, "unsafe"), 10);
            assert_eq!(source.matches("#[allow(unsafe_code)]").count(), 2);
            assert!(!source.contains("global_allocator"));
            fingerprint(
                &source,
                "668320d1f8caf82018331e0325571b16f69423794d4bff888a410a2c416d3702",
            );
        } else {
            assert_eq!(words(&source, "unsafe"), 0, "{relative}");
            assert!(!source.contains("allow(unsafe_code"), "{relative}");
            assert!(!source.contains("global_allocator"), "{relative}");
        }
        if relative == "tests/support/allocation_registry.rs" {
            fingerprint(
                &source,
                "a3fb5de818f0d5240fc32a3a915337412a915a698bae54e34fd86665bc507caf",
            );
        }
        if relative == "tests/support/allocation_counter.rs" {
            fingerprint(
                &source,
                "14e2ef08d8dea85709f5d86e2a60e0b2e965903f5c001e9a46fa9bdecfe9e7d1",
            );
        }
        if !matches!(
            relative,
            "tests/native_alloc_probe.rs"
                | "tests/support/native_allocator_bridge.rs"
                | "tests/support/native_allocation_controls.rs"
        ) {
            for forbidden in [
                "native_alloc_probe",
                "native_allocator_bridge",
                "native_allocation_controls",
                "TD_MTA_NATIVE_REGISTRY",
                "__wrap_",
                "__real_",
            ] {
                assert!(!source.contains(forbidden), "{relative}: {forbidden}");
            }
        }
        if relative.starts_with("src/") {
            assert!(!source.contains("allocation_registry"), "{relative}");
        }
        if !matches!(
            relative,
            "tests/rust_alloc_probe.rs"
                | "tests/support/allocation_shim.rs"
                | "tests/support/allocation_counter.rs"
        ) {
            for forbidden in [
                "allocation_shim",
                "allocation_counter",
                "rust_alloc_probe",
                "TD_MTA_ALLOCATION_COUNTERS",
            ] {
                assert!(!source.contains(forbidden), "{relative}: {forbidden}");
            }
        }
    }
}

#[test]
fn allocation_surface_is_separate_and_exact() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let library = include_str!("../src/lib.rs");
    assert_eq!(library.matches("#![forbid(unsafe_code)]").count(), 1);
    assert_eq!(
        include_str!("../src/main.rs")
            .matches("#![forbid(unsafe_code)]")
            .count(),
        1
    );
    let probe = include_str!("rust_alloc_probe.rs");
    assert_eq!(probe.matches("#![cfg(test)]").count(), 1);
    assert_eq!(probe.matches("#![deny(unsafe_code)]").count(), 1);
    assert_eq!(probe.matches("mod allocation_shim;").count(), 1);
    assert!(include_str!("../Cargo.toml")
        .contains("[[test]]\nname = \"rust_alloc_probe\"\nharness = false\n"));
    let native = include_str!("native_alloc_probe.rs");
    assert_eq!(native.matches("#![cfg(test)]").count(), 1);
    assert_eq!(native.matches("#![deny(unsafe_code)]").count(), 1);
    for module in [
        "allocation_registry",
        "native_allocator_bridge",
        "native_allocation_controls",
    ] {
        assert!(native.contains(&format!(
            "#[cfg(td_native_alloc_probe)]\n#[path = \"support/{module}.rs\"]\nmod {module};"
        )));
    }
    assert!(include_str!("../Cargo.toml")
        .contains("[[test]]\nname = \"native_alloc_probe\"\nharness = false\n"));
    scan(root, &root.join("src"));
    scan(root, &root.join("tests"));
    scan(root, &root.join("tools"));
    scan(root, &root.join("examples"));
}
