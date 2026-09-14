#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]
use std::collections::BTreeSet;
use std::path::Path;
#[test]
fn source_inventory_and_parser_boundary_are_closed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let expected = [
        "budget.rs",
        "collector.rs",
        "contributors.rs",
        "devices.rs",
        "hierarchy.rs",
        "history.rs",
        "identities.rs",
        "lib.rs",
        "linux_read.rs",
        "main.rs",
        "model.rs",
        "parsers.rs",
        "snapshot.rs",
        "worker.rs",
        "projection.rs",
        "format.rs",
        "view.rs",
        "plots.rs",
        "device_selection.rs",
        "search.rs",
        "ui.rs",
        "window.rs",
        "ranking.rs",
        "control.rs",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    let mut actual = BTreeSet::new();
    for entry in std::fs::read_dir(root.join("src")).unwrap() {
        let entry = entry.unwrap();
        assert!(entry.file_type().unwrap().is_file());
        actual.insert(entry.file_name().into_string().unwrap());
    }
    assert_eq!(
        actual.iter().map(String::as_str).collect::<BTreeSet<_>>(),
        expected
    );
    for name in ["lib.rs", "main.rs"] {
        assert!(std::fs::read_to_string(root.join("src").join(name))
            .unwrap()
            .contains("#![forbid(unsafe_code)]"));
    }
    for name in [
        "parsers.rs",
        "hierarchy.rs",
        "contributors.rs",
        "projection.rs",
        "format.rs",
        "view.rs",
        "plots.rs",
        "device_selection.rs",
        "search.rs",
        "ui.rs",
        "ranking.rs",
        "control.rs",
    ] {
        let source = std::fs::read_to_string(root.join("src").join(name)).unwrap();
        for denied in [
            "std::fs",
            "std::process",
            "std::os",
            "std::time",
            "std::net",
            "std::env",
            "std::thread",
            "#[path",
            "include!(",
            "include_str!(",
            "td_ui::wayland",
            "td_ui::client",
            "td_ui::control_socket",
            "td_ui::control_worker",
        ] {
            assert!(!source.contains(denied), "{name}: {denied}");
        }
    }
    assert!(!root.join("build.rs").exists());
}
