#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Source-level contracts the compiler cannot express: the crate's file
//! inventory, and which toolkit modules its files may name. td-ui/DESIGN.md
//! requires each consumer to pin the second in its own confinement tests.
//! The front end only renders this increment, so no file names
//! `td_ui::wayland` or `td_ui::client`; increment 6(b) adds the turn loop
//! that makes it a live client and revises this test in the same landing.

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
    let expected: BTreeSet<String> = ["lib.rs", "main.rs", "welcome.rs"]
        .iter()
        .map(|name| name.to_string())
        .collect();
    let mut actual = BTreeSet::new();
    for entry in std::fs::read_dir(root.join("src")).unwrap() {
        let entry = entry.unwrap();
        assert!(entry.file_type().unwrap().is_file(), "no nested source");
        let name = entry.file_name().into_string().unwrap();
        let text = std::fs::read_to_string(entry.path()).unwrap();
        // No live-client toolkit module is named before increment 6(b): the
        // page is pure rendering over the raster and the chrome bands.
        for module in ["td_ui::wayland", "td_ui::client"] {
            assert!(!text.contains(module), "{name} names {module} before 6(b)");
        }
        actual.insert(name);
    }
    assert_eq!(actual, expected, "unexpected or missing source file");
}
