#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::path::Path;

fn raw_module_tokens(text: &str) -> usize {
    identifier_count(text, "sys")
}

fn identifier_count(text: &str, identifier: &str) -> usize {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|token| *token == identifier)
        .count()
}

#[test]
fn source_inventory_and_allowances_are_closed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(!root.join("build.rs").exists());
    let expected: BTreeSet<_> = [
        "clipboard.rs",
        "data.rs",
        "dialog.rs",
        "files.rs",
        "fill.rs",
        "keys.rs",
        "keyboard.rs",
        "layout.rs",
        "lib.rs",
        "main.rs",
        "menu.rs",
        "model.rs",
        "pointer.rs",
        "render.rs",
        "replay.rs",
        "seat.rs",
        "session.rs",
        "sys.rs",
        "text.rs",
        "transfer.rs",
        "ui.rs",
        "wayland.rs",
        "xkb.rs",
        "xkb_compat.rs",
        "xkb_keys.rs",
        "xkb_symbols.rs",
        "xkb_syntax.rs",
    ]
    .into_iter()
    .map(str::to_string)
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
        if !matches!(
            name.as_str(),
            "keyboard.rs"
                | "xkb.rs"
                | "xkb_syntax.rs"
                | "xkb_keys.rs"
                | "xkb_symbols.rs"
                | "xkb_compat.rs"
                | "seat.rs"
                | "wayland.rs"
        ) {
            for module in [
                "keyboard",
                "xkb",
                "xkb_syntax",
                "xkb_keys",
                "xkb_symbols",
                "xkb_compat",
            ] {
                assert_eq!(
                    identifier_count(&text, module),
                    usize::from(name == "lib.rs"),
                    "compiler access outside input adapter: {name}"
                );
            }
            for interface in ["wl_seat", "wl_keyboard"] {
                assert!(
                    !text.contains(interface),
                    "input binding outside the adapter: {name}"
                );
            }
        }
        let budget = match name.as_str() {
            "lib.rs" | "main.rs" => 1,
            "sys.rs" => 4,
            _ => 0,
        };
        assert_eq!(
            text.matches("unsafe").count(),
            budget,
            "keyword count in {name}"
        );
        assert!(
            !text.contains("cfg_attr"),
            "conditional allowance in {name}"
        );
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(!compact.contains("include!("), "generated source in {name}");
        let paths = compact.matches("#[path=").count();
        assert_eq!(
            paths,
            if name == "lib.rs" { 3 } else { 0 },
            "source paths in {name}"
        );
        if name == "lib.rs" {
            assert!(compact.starts_with("#![deny(unsafe_code)]"));
            for (file, declaration) in [
                ("font.rs", "pubmodfont;"),
                ("font_data.rs", "modfont_data;"),
                ("wire.rs", "modwire;"),
            ] {
                assert!(compact.contains(&format!(
                    "#[path=\"../../td-compositor/src/{file}\"]{declaration}"
                )));
                let shared =
                    std::fs::read_to_string(root.join("../td-compositor/src").join(file)).unwrap();
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
                        "partial type validation through shared source: {file}"
                    );
                }
                for interface in ["wl_seat", "wl_keyboard"] {
                    assert!(
                        !shared.contains(interface),
                        "input binding through shared source: {file}"
                    );
                }
                assert!(!shared.contains("unsafe"));
                assert_eq!(
                    raw_module_tokens(&shared),
                    0,
                    "shared raw-module access in {file}"
                );
                let shared: String = shared.chars().filter(|c| !c.is_whitespace()).collect();
                assert!(!shared.contains("#[path="));
                assert!(!shared.contains("include!("));
                assert!(!shared.contains("cfg_attr"));
            }
        }
        assert_eq!(
            raw_module_tokens(&text),
            match name.as_str() {
                "lib.rs" => 1,
                "files.rs" => 1,
                "wayland.rs" => 4,
                "transfer.rs" => 2,
                _ => 0,
            },
            "unrostered raw-module access in {name}"
        );
    }
    assert_eq!(actual, expected);
}

#[test]
fn complete_raw_layer_and_production_callers_are_pinned() {
    let raw = include_str!("../src/sys.rs");
    let hash = raw.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    });
    assert_eq!(
        hash, 0xc1b0a580e9da8ee8,
        "review the complete raw layer before updating its fingerprint"
    );
    for pin in [
        "const SYS_SENDMSG: usize = 46;",
        "const SYS_RECVMSG: usize = 47;",
        "const SYS_FCNTL: usize = 72;",
        "const SYS_FLISTXATTR: usize = 196;",
        "syscall3(SYS_FLISTXATTR, file.as_raw_fd() as usize, 0, 0)",
        "const F_DUPFD_CLOEXEC: usize = 1030;",
        "const F_GETFL: usize = 3;",
        "const F_SETFL: usize = 4;",
        "const O_NONBLOCK: usize = 0o4000;",
        "const O_ACCMODE: usize = 3;",
        "#[allow(unsafe_code)]\nfn syscall3(",
        "#[allow(unsafe_code)]\nfn adopt(",
    ] {
        assert!(raw.contains(pin), "{pin}");
    }
    assert!(!raw.contains("#![allow("));
    assert_eq!(raw.matches("core::arch::asm!").count(), 1);
    assert_eq!(raw.matches("OwnedFd::from_raw_fd").count(), 1);
    let transfer = include_str!("../src/transfer.rs");
    assert_eq!(transfer.matches("crate::sys::").count(), 2);
    assert_eq!(
        transfer
            .matches("crate::sys::Destination::new(fd)?")
            .count(),
        1
    );
    assert_eq!(
        transfer
            .matches("destination: crate::sys::Destination,")
            .count(),
        1
    );
    let files = include_str!("../src/files.rs");
    assert_eq!(files.matches("crate::sys::has_attributes(file)").count(), 1);
    assert_eq!(files.matches("crate::sys::").count(), 1);
    for pin in [
        "const O_NOFOLLOW: i32 = 0o400000;",
        "const O_NONBLOCK: i32 = 0o4000;",
        "const O_DIRECTORY: i32 = 0o200000;",
        ".custom_flags(O_NOFOLLOW | O_NONBLOCK)",
        "temporary.file.sync_all()?;",
        "location.parent.sync_all()?;",
        "fs::rename(&temporary.path, &location.path)?;",
        "fs::hard_link(&temporary.path, &location.path)",
    ] {
        assert!(files.contains(pin), "file transaction pin: {pin}");
    }
    let adapter = include_str!("../src/wayland.rs");
    assert_eq!(adapter.matches("Keymap::parse(source)").count(), 1);
    assert_eq!(adapter.matches("File::from(fd)").count(), 1);
    assert_eq!(adapter.matches("read_keymap(fd, format, size)").count(), 1);
    assert!(adapter.contains("file.read_exact_at(&mut bytes, 0)"));
    assert_eq!(adapter.matches("crate::sys::").count(), 4);
    for call in [
        "crate::sys::inherited(fd)",
        "crate::sys::send_file(&self.stream, suffix, file)",
        "crate::sys::receive(&self.stream, &mut self.read)",
        "crate::sys::receive_for_test(peer, &mut buf)",
    ] {
        assert_eq!(adapter.matches(call).count(), 1, "{call}");
    }
}
