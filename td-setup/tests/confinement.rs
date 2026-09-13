#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Source-level contracts the compiler cannot express: the crate's file
//! inventory, and which toolkit modules its files may name. td-ui/DESIGN.md
//! requires each consumer to pin the second in its own confinement tests.
//! `window` is the crate's only Wayland client: it alone names
//! `td_ui::wayland` and `td_ui::client`, so the pages stay pure rendering
//! over the raster and the chrome bands and cannot quietly reach the
//! transport or the turn loop.

use std::collections::BTreeSet;
use std::path::Path;

#[test]
fn source_inventory_and_toolkit_access_are_closed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(!root.join("build.rs").exists(), "no build script");
    // The crate root forbids unsafe for the whole crate.
    assert!(
        std::fs::read_to_string(root.join("src/lib.rs"))
            .unwrap()
            .contains("#![forbid(unsafe_code)]"),
        "crate root forbids unsafe"
    );
    let expected: BTreeSet<String> = ["lib.rs", "main.rs", "welcome.rs", "window.rs"]
        .iter()
        .map(|name| name.to_string())
        .collect();
    let mut actual = BTreeSet::new();
    for entry in std::fs::read_dir(root.join("src")).unwrap() {
        let entry = entry.unwrap();
        assert!(entry.file_type().unwrap().is_file(), "no nested source");
        let name = entry.file_name().into_string().unwrap();
        let text = std::fs::read_to_string(entry.path()).unwrap();
        // Only `window` reaches the live client; every other file, the pages
        // included, stays off the transport and the turn loop.
        if name != "window.rs" {
            for module in ["td_ui::wayland", "td_ui::client"] {
                assert!(
                    !text.contains(module),
                    "{name} names {module}; only window may"
                );
            }
        }
        actual.insert(name);
    }
    assert_eq!(actual, expected, "unexpected or missing source file");
    // The client boundary is real: `window` names both the transport and the
    // client, so removing either from it would be caught here, not silently.
    let window = std::fs::read_to_string(root.join("src/window.rs")).unwrap();
    assert!(
        window.contains("td_ui::wayland") && window.contains("td_ui::client"),
        "window is the crate's Wayland client boundary"
    );
}

/// The inventory above walks `src`; a test file could still mount a sibling
/// crate's source by `#[path]` and so smuggle a second copy of, say, a
/// toolkit module. The test tree is held to the same line: every `#[path]`
/// mount stays inside this crate's `tests`, so the native harness under
/// `tests/support` cannot reach up into td-ui or td-compositor. Fixtures read
/// by `include_str!` are data, not modules, and are not constrained here.
#[test]
fn test_files_mount_no_sibling_source() {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests"),
        &mut files,
    );
    assert!(files.len() >= 3, "{files:?}");
    let mut mounted = false;
    for file in files {
        // The guard names these attribute spellings in its own logic and
        // comments; it declares no modules, so skip it rather than self-match.
        if file.file_name().and_then(|name| name.to_str()) == Some("confinement.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        // A raw-string path attribute (`path=r"..."` or `path=r#"..."#`, direct
        // or via `cfg_attr`) would carry a `..` past the value scan below; the
        // crate has no use for one, so forbid the spelling outright.
        assert!(
            !compact.contains("path=r"),
            "raw-string #[path] mount in {}",
            file.display()
        );
        // Every plain path attribute value, `#[path="..."]` or a conditional
        // `#[cfg_attr(_, path="...")]`, wherever `..` sits in it:
        // `support/../../x` reaches as far as `../../x`.
        for mount in compact.split("path=\"").skip(1) {
            mounted = true;
            let value = mount.split('"').next().unwrap_or(mount);
            assert!(
                !value.contains("..") && !value.starts_with('/'),
                "source mounted from outside the test tree: {} ({value})",
                file.display()
            );
        }
    }
    // The native harness is mounted by path; if that mount ever disappears the
    // check above would pass vacuously, so require at least one to exist.
    assert!(mounted, "no #[path] mount found in the test tree");
}
