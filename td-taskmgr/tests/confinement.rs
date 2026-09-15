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
        "actions.rs",
        "action_plan.rs",
        "action_linux.rs",
        "action_worker.rs",
        "action_ui.rs",
        "signal_sys.rs",
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
    assert!(std::fs::read_to_string(root.join("src/lib.rs"))
        .unwrap()
        .contains("#![deny(unsafe_code)]"));
    assert!(std::fs::read_to_string(root.join("src/main.rs"))
        .unwrap()
        .contains("#![forbid(unsafe_code)]"));
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
        "actions.rs",
        "action_plan.rs",
        "action_ui.rs",
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

#[test]
fn process_signal_surface_is_fixed_and_private() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read_to_string(root.join("src/signal_sys.rs")).unwrap();
    let hash = raw.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    assert_eq!(hash, 0x99692c0034621d25);
    assert_eq!(raw.matches("#[allow(unsafe_code)]").count(), 1);
    assert_eq!(raw.matches("unsafe {").count(), 1);
    assert_eq!(raw.matches("std::arch::asm!").count(), 1);
    for pinned in [
        "let mut result = 424i64;",
        "inlateout(\"rax\") result",
        "in(\"rdi\") i64::from(directory.as_raw_fd())",
        "in(\"rsi\") signal.map(number).unwrap_or(0)",
        "in(\"rdx\") 0usize",
        "in(\"r10\") 0usize",
        "lateout(\"rcx\") _",
        "lateout(\"r11\") _",
        "options(nostack)",
    ] {
        assert!(raw.contains(pinned), "{pinned}");
    }
    let adapter = std::fs::read_to_string(root.join("src/action_linux.rs")).unwrap();
    let production = adapter.split("#[cfg(test)]").next().unwrap();
    assert_eq!(production.matches("signal_sys::probe(").count(), 1);
    assert_eq!(production.matches("signal_sys::send(").count(), 1);
    assert!(production.contains("File::open(\"/proc/self\")"));
    assert!(production.contains("action_plan::caller(status, self.own_pid)"));
    assert!(production.contains("action_plan::protected(status, self.own_pid)"));
    for entry in std::fs::read_dir(root.join("src")).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_str().unwrap();
        let text = std::fs::read_to_string(entry.path()).unwrap();
        if !matches!(name, "lib.rs" | "signal_sys.rs" | "action_linux.rs") {
            assert!(!text.contains("signal_sys"), "{name}");
        }
        if name != "signal_sys.rs" {
            for denied in [
                "unsafe {",
                "allow(unsafe_code)",
                "asm!",
                "from_raw_fd",
                "extern \"C\"",
            ] {
                assert!(!text.contains(denied), "{name}: {denied}");
            }
        }
    }
}
