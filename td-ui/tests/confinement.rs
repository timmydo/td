#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! Source-level contracts the compiler cannot express: what the crate is
//! made of, what it mounts or embeds from elsewhere, what its pure modules
//! never touch, and the complete raw layer beneath the Wayland transport.

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

const PURE: &[&str] = &[
    "atlas.rs",
    "charts.rs",
    "chrome.rs",
    "confirmations.rs",
    "control.rs",
    "coverage.rs",
    "data.rs",
    "driven.rs",
    "editor.rs",
    "editor_clipboard.rs",
    "editor_dialog.rs",
    "editor_error.rs",
    "editor_fill.rs",
    "editor_keys.rs",
    "editor_layout.rs",
    "editor_model.rs",
    "editor_render.rs",
    "editor_search.rs",
    "editor_text.rs",
    "entry_model.rs",
    "face.rs",
    "finder.rs",
    "hint.rs",
    "keyboard.rs",
    "keys.rs",
    "links.rs",
    "list_model.rs",
    "menus.rs",
    "messages.rs",
    "pointer.rs",
    "raster.rs",
    "repeat.rs",
    "sfnt.rs",
    "split.rs",
    "theme.rs",
    "tree_table.rs",
    "tree_table_geometry.rs",
    "tree_table_model.rs",
    "tree_table_paint.rs",
    "typeface.rs",
    "vt_keys.rs",
    "xkb.rs",
    "xkb_compat.rs",
    "xkb_keys.rs",
    "xkb_symbols.rs",
    "xkb_syntax.rs",
];

/// The terminal model, its renderer and its terminfo entry: pure like
/// `PURE`, but their tests are specification files mounted beside them
/// (or, for terminfo, two test modules), so the production text is what
/// precedes the first column-zero `#[cfg(test)]` and the tail is pinned
/// exactly.
const TERMINAL: [(&str, &str); 3] = [
    (
        "vt.rs",
        "#[cfg(test)]\n#[path = \"vt_spec.rs\"]\nmod spec;\n",
    ),
    (
        "vt_render.rs",
        "#[cfg(test)]\n#[path = \"vt_render_spec.rs\"]\nmod spec;\n",
    ),
    ("vt_terminfo.rs", ""),
];

/// Specification files compiled only as those modules' tests.
const TERMINAL_SPECS: [&str; 2] = ["vt_spec.rs", "vt_render_spec.rs"];

#[test]
fn source_inventory_and_shared_mounts_are_closed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(!root.join("build.rs").exists());
    let expected: BTreeSet<String> = PURE
        .iter()
        .chain(
            [
                "client.rs",
                "clipboard.rs",
                "control_socket.rs",
                "control_worker.rs",
                "lib.rs",
                "face_file.rs",
                "notices.rs",
                "open.rs",
                "pinned_face.rs",
                "pty.rs",
                "replay.rs",
                "sys.rs",
                "theme_file.rs",
                "wayland.rs",
                "window.rs",
                "xdg.rs",
            ]
            .iter(),
        )
        .chain(TERMINAL.iter().map(|(name, _)| name))
        .chain(TERMINAL_SPECS.iter())
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
        assert!(
            !compact.contains("cfg_attr"),
            "conditional allowance in {name}"
        );
        assert_eq!(
            text.matches("unsafe").count(),
            match name.as_str() {
                "lib.rs" => 1,
                "sys.rs" => 6,
                _ => 0,
            },
            "unsafe keyword in {name}"
        );
        // The raw module is named by the crate root's private declaration,
        // by the transport's two imports and five wrapper calls, by the
        // clipboard's import and five status calls and by the PTY's import,
        // four ioctl wrapper calls and its spawn's session hook; no other
        // module, shared source or
        // test-support reader reaches it.
        // The socket's pinned procfs pathname has a `sys` segment that is
        // not raw-module access.
        let raw_text = if name == "control_socket.rs" {
            let literal = "\"/proc/sys/kernel/overflowuid\"";
            assert_eq!(text.matches(literal).count(), 1);
            text.replacen(literal, "\"\"", 1)
        } else {
            text.clone()
        };
        assert_eq!(
            identifier_count(&raw_text, "sys"),
            match name.as_str() {
                "lib.rs" => 1,
                "wayland.rs" => 7,
                "clipboard.rs" => 6,
                "pty.rs" => 6,
                _ => 0,
            },
            "raw-module access in {name}"
        );
        assert_eq!(
            compact.matches("#[path=").count(),
            match name.as_str() {
                "lib.rs" => 6,
                // The two specifications, the engine's SHA-256 the model's
                // specification checks the libvterm import with, and the
                // fonts the renderer's outline oracles encode.
                "vt.rs" | "vt_render.rs" | "vt_spec.rs" | "vt_render_spec.rs" => 1,
                _ => 0,
            },
            "source paths in {name}"
        );
        if name == "vt_spec.rs" {
            assert!(compact.contains(
                "#[allow(dead_code)]#[path=\"../../engine/src/sha256.rs\"]modmigration_sha256;"
            ));
        }
        if name == "vt_render_spec.rs" {
            assert!(compact.contains("#[path=\"../tests/fonts/mod.rs\"]modfonts;"));
        }
        if TERMINAL_SPECS.contains(&name.as_str()) {
            assert!(
                !text.contains("#[cfg(test)]"),
                "a specification is test code throughout: {name}"
            );
        }
        if let Some((_, tail)) = TERMINAL.iter().find(|(file, _)| *file == name.as_str()) {
            let (production, tests) = text
                .split_once("\n#[cfg(test)]\n")
                .unwrap_or((text.as_str(), ""));
            if tail.is_empty() {
                // Two tail test modules and nothing else at column zero.
                let modules: Vec<&str> = tests
                    .lines()
                    .chain(["#[cfg(test)]"])
                    .filter(|line| !line.is_empty() && !line.starts_with([' ', '}', '/']))
                    .collect();
                assert_eq!(
                    modules,
                    [
                        "mod tests {",
                        "#[cfg(test)]",
                        "mod effects {",
                        "#[cfg(test)]"
                    ],
                    "test tail of {name}"
                );
            } else {
                assert_eq!(
                    format!("#[cfg(test)]\n{tests}"),
                    *tail,
                    "test tail of {name}"
                );
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
                "#[path",
                "mod ",
            ] {
                assert!(
                    !production.contains(ambient),
                    "ambient access `{ambient}` in terminal module {name}"
                );
            }
        }
        assert_eq!(
            text.matches(".pop_descriptor()").count(),
            match name.as_str() {
                "client.rs" => 3,
                "wayland.rs" => 1,
                _ => 0,
            },
            "pops: the transport's own test, the client's accessor, keymap reader and send: {name}"
        );
        for support in [
            ".unconfigure(",
            ".input_mut(",
            ".take_for_test(",
            ".generation_for_test(",
        ] {
            assert_eq!(
                text.matches(support).count(),
                0,
                "the client's test support {support} is not called in {name}"
            );
        }
        if name == "face_file.rs" {
            // One file opened, checked and bounded before it is read; a
            // search that only lists directories and looks at the regular
            // style's name, over places built from the image's directory,
            // the installed one and the XDG font roots; three environment
            // values; nothing written.
            assert!(text.contains("pub const DIR: &str = \"/etc/fonts/jetbrains-mono-nerd\";"));
            assert!(text.contains("pub const INSTALLED: &str = \"fonts/jetbrains-mono-nerd\";"));
            assert_eq!(compact.matches(".open(&path)").count(), 1);
            assert!(compact
                .contains("OpenOptions::new().read(true).custom_flags(O_NONBLOCK).open(&path)"));
            assert!(text.contains("const O_NONBLOCK: i32 = 0o4000;"));
            assert_eq!(compact.matches("file.metadata()").count(), 1);
            assert_eq!(compact.matches("fs::metadata(").count(), 2);
            assert_eq!(
                compact.matches("fs::metadata(dir.join(REGULAR))").count(),
                1
            );
            assert_eq!(compact.matches("fs::read_dir(").count(), 1);
            assert!(compact.contains(".take(MAX_FONT_BYTESasu64+1)"));
            let joined: BTreeSet<&str> = compact
                .match_indices(".join(")
                .filter_map(|(at, _)| compact.get(at + ".join(".len()..)?.split(')').next())
                .collect();
            assert_eq!(
                joined,
                BTreeSet::from([
                    "\",\"", // the error's list of places, a string join
                    "\".fonts\"",
                    "\".local/share\"",
                    "\"fonts\"",
                    "INSTALLED",
                    "REGULAR",
                    "name",
                ]),
                "the paths face_file builds"
            );
            assert_eq!(compact.matches("env::").count(), 4);
            for read in ["HOME", "XDG_DATA_HOME", "XDG_DATA_DIRS"] {
                assert_eq!(
                    compact
                        .matches(&format!("std::env::var_os(\"{read}\")"))
                        .count(),
                    1,
                    "{read}"
                );
            }
            assert_eq!(compact.matches("std::env::split_paths(").count(), 1);
            for absent in [
                "canonicalize",
                "eprintln!",
                "File::open(",
                ".write(",
                ".append(",
                ".create(",
                ".truncate(",
                "fs::write",
                "fs::remove",
                "fs::create",
                "fs::rename",
                "File::create",
                "set_var",
            ] {
                assert!(!compact.contains(absent), "{absent} in face_file.rs");
            }
        }
        if name == "theme_file.rs" {
            // Two environment values for the path; one file opened,
            // checked and bounded before it is read; one directory made
            // and locked, and one private sibling renamed over the file it
            // replaces.
            assert_eq!(compact.matches("env::").count(), 2);
            for read in ["XDG_CONFIG_HOME", "HOME"] {
                assert_eq!(
                    compact
                        .matches(&format!("std::env::var_os(\"{read}\")"))
                        .count(),
                    1,
                    "{read}"
                );
            }
            assert!(compact
                .contains("OpenOptions::new().read(true).custom_flags(O_NONBLOCK).open(path)"));
            assert!(text.contains("const O_NONBLOCK: i32 = 0o4000;"));
            assert!(compact.contains(".take(MAX_FILE_BYTESasu64+1)"));
            assert!(compact.contains(
                "OpenOptions::new().write(true).create_new(true).mode(0o600).open(&sibling)"
            ));
            assert!(compact.contains("DirBuilder::new().recursive(true).mode(0o700).create(dir)"));
            // The read, the sibling, and the directory locked around the
            // replacement.
            assert_eq!(compact.matches(".open(").count(), 3);
            assert!(compact.contains("OpenOptions::new().read(true).open(dir)"));
            assert_eq!(compact.matches("lock.try_lock()").count(), 1);
            assert!(!compact.contains(".lock()"), "a write never waits");
            assert_eq!(compact.matches("fs::rename(&sibling,path)").count(), 1);
            assert_eq!(compact.matches("fs::remove_file(&sibling)").count(), 2);
            for absent in [
                "canonicalize",
                "eprintln!",
                "File::open(",
                "File::create",
                ".append(",
                ".truncate(",
                "fs::write",
                "fs::read",
                "remove_dir",
                "set_var",
                "set_permissions",
            ] {
                assert!(!compact.contains(absent), "{absent} in theme_file.rs");
            }
        }
        if name == "pinned_face.rs" {
            // Reads only through face_file.
            for absent in ["File", "fs::", "env::", "eprintln!", "OpenOptions"] {
                assert!(!compact.contains(absent), "{absent} in pinned_face.rs");
            }
        }
        if name == "lib.rs" {
            assert!(compact.starts_with("#![deny(unsafe_code)]"));
            assert!(!compact.contains("#![allow("));
            assert!(compact.contains("modsys;"), "the raw module is declared");
            assert!(!compact.contains("pubmodsys"), "the raw module is private");
            assert!(compact.contains("pubmodwayland;"));
            for terminal in ["pty", "vt", "vt_keys", "vt_render", "vt_terminfo"] {
                assert!(compact.contains(&format!("pubmod{terminal};")));
            }
            for (file, declaration) in [
                ("filter.rs", "pubmodfilter;"),
                ("font.rs", "pubmodfont;"),
                ("font_data.rs", "modfont_data;"),
                ("proc_status.rs", "pubmodproc_status;"),
                ("reportable.rs", "pubmodreportable;"),
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
                assert!(
                    !shared.contains("unsafe"),
                    "unsafe through shared source: {file}"
                );
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
        if name == "notices.rs" {
            // Data, not code: three embedded texts from the assets directory
            // beside the face, the outline face's pointer, and nothing else.
            let items: Vec<&str> = text
                .lines()
                .filter(|line| !line.is_empty() && !line.starts_with("//"))
                .collect();
            let assets = "include_str!(\"../../td-compositor/assets/";
            assert_eq!(
                items,
                [
                    format!("pub const FONT_PROVENANCE: &str = {assets}PROVENANCE\");"),
                    format!("pub const FONT_COPYING: &str = {assets}unifont-COPYING\");"),
                    format!("pub const FONT_LICENSE: &str = {assets}unifont-OFL-1.1.txt\");"),
                    "pub const OUTLINE_FACE: &str = \"The outline face is JetBrains Mono Nerd Font Mono from the Nerd Fonts v3.5.1 release, read from /etc/fonts/jetbrains-mono-nerd, where its licences ship beside it: OFL.txt, README.md (each merged icon set and its licence) and licenses/. Run elsewhere, a program reads the same files from the user's font directory, where ./install-fonts puts them, or else a copy the host packaged, which may be another release and keep its licences elsewhere.\\n\";".to_string(),
                ],
                "notices carry the three texts beside the face, the outline pointer and nothing else"
            );
        }
        if name == "finder.rs" {
            // The finder paints and handles events without allocating: its
            // status counts stream through a digit iterator, so no
            // formatting or collecting appears in the module at all.
            for allocating in [
                "format!(",
                ".to_string()",
                ".to_owned()",
                "String::from(",
                "vec![",
                ".collect()",
                "Box::new(",
            ] {
                assert!(
                    !text.contains(allocating),
                    "allocation `{allocating}` in the finder"
                );
            }
        }
        // Outside tests the environment is read in four places: the
        // opener's `BROWSER`, the face search's three directory values
        // and their list's split, the theme file's two, pinned above, and
        // the base-directory lookup's variable and `HOME`.
        let production = text.split("#[cfg(test)]").next().unwrap_or_default();
        assert_eq!(
            production.matches("std::env").count(),
            match name.as_str() {
                "open.rs" => 1,
                "face_file.rs" => 4,
                "theme_file.rs" => 2,
                "xdg.rs" => 2,
                _ => 0,
            },
            "environment access in {name}"
        );
        if name == "xdg.rs" {
            // Read only, never set, and only the two values the rule takes.
            let flat = crate::compact(production);
            assert!(flat.contains("std::env::var_os(base.variable())"));
            assert!(flat.contains("std::env::var_os(\"HOME\")"));
            for absent in ["std::fs", "std::process", "set_var", "remove_var"] {
                assert!(!flat.contains(absent), "{absent} in xdg.rs");
            }
        }
        if name == "open.rs" {
            assert!(production.contains("std::env::var(\"BROWSER\")"));
            assert!(production.contains(".env_remove(\"WAYLAND_SOCKET\")"));
            assert!(production.contains(".env(\"WAYLAND_DISPLAY\", display)"));
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
fn complete_raw_layer_and_its_sole_caller_are_pinned() {
    let raw = include_str!("../src/sys.rs");
    let hash = raw.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    });
    assert_eq!(
        hash, 0x392769aeae75189f,
        "review the complete raw layer before updating its fingerprint"
    );
    for pin in [
        "const SYS_SENDMSG: usize = 46;",
        "const SYS_RECVMSG: usize = 47;",
        "const SYS_FCNTL: usize = 72;",
        "const SYS_POLL: usize = 7;",
        "const SYS_IOCTL: usize = 16;",
        "const SYS_SETSID: usize = 112;",
        "const POLLIN: i16 = 1;",
        "const TIOCSPTLCK: usize = 0x4004_5431;",
        "const TIOCGPTPEER: usize = 0x5441;",
        "const TIOCSWINSZ: usize = 0x5414;",
        "const TIOCGWINSZ: usize = 0x5413;",
        "const TIOCSCTTY: usize = 0x540e;",
        "const PTY_PEER_FLAGS: usize = 0o2 | 0o400 | 0o2_000_000;",
        "const F_DUPFD_CLOEXEC: usize = 1030;",
        "const F_GETFL: usize = 3;",
        "const F_SETFL: usize = 4;",
        "const SOL_SOCKET: i32 = 1;",
        "const SCM_RIGHTS: i32 = 1;",
        "const MSG_CTRUNC: i32 = 8;",
        "const MSG_NOSIGNAL: usize = 0x4000;",
        "const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;",
        "const HEADER: usize = 16;",
        "const CONTROL: usize = 128;",
        "in(\"r10\") a4,",
        "in(\"r8\") a5,",
        "syscall5(number, a1, a2, a3, 0, 0)",
        "syscall3(SYS_FCNTL, fd as usize, F_DUPFD_CLOEXEC, 3)",
        "syscall3(SYS_FCNTL, file.as_raw_fd() as usize, F_GETFL, 0)",
        "result(syscall3(\n        SYS_FCNTL,\n        file.as_raw_fd() as usize,\n        F_SETFL,\n        flags,\n    ))",
        "pub(crate) fn status(file: &File) -> io::Result<usize>",
        "pub(crate) fn set_status(file: &File, flags: usize) -> io::Result<()>",
        "#[allow(unsafe_code)]\nfn syscall5(",
        "#[allow(unsafe_code)]\nfn adopt(",
        "pub(crate) fn inherited(fd: i32) -> io::Result<UnixStream>",
        "pub(crate) fn receive(stream: &UnixStream, bytes: &mut [u8])",
        "pub(crate) fn send_file(stream: &UnixStream, bytes: &[u8], file: &File)",
        "pub(crate) fn readable(\n    stream: &UnixStream,\n    waker: &UnixDatagram,\n    timeout_ms: u16,\n) -> io::Result<[bool; 2]>",
        "SYS_POLL,\n        fds.as_mut_ptr() as usize,\n        fds.len(),\n        usize::from(timeout_ms),",
        "events: POLLIN,",
        "pub(crate) fn unlock_pty(master: &File) -> io::Result<()>",
        "let unlocked: i32 = 0;",
        "SYS_IOCTL,\n        master.as_raw_fd() as usize,\n        TIOCSPTLCK,\n        (&unlocked as *const i32) as usize,",
        "pub(crate) fn pty_peer(master: &File) -> io::Result<File>",
        "SYS_IOCTL,\n        master.as_raw_fd() as usize,\n        TIOCGPTPEER,\n        PTY_PEER_FLAGS,",
        "Ok(File::from(adopt(fd)))",
        "pub(crate) fn set_window_size(terminal: &File, words: [u16; 4]) -> io::Result<()>",
        "SYS_IOCTL,\n        terminal.as_raw_fd() as usize,\n        TIOCSWINSZ,\n        (&words as *const [u16; 4]) as usize,",
        "pub(crate) fn window_size(terminal: &File) -> io::Result<[u16; 4]>",
        "let mut words = [0u16; 4];",
        "SYS_IOCTL,\n        terminal.as_raw_fd() as usize,\n        TIOCGWINSZ,\n        (&mut words as *mut [u16; 4]) as usize,",
        "fn lead_session_on_stdin() -> io::Result<()> {\n    result(syscall3(SYS_SETSID, 0, 0, 0))?;\n    result(syscall3(SYS_IOCTL, 0, TIOCSCTTY, 0)).map(|_| ())\n}",
        "#[allow(unsafe_code)]\npub(crate) fn lead_session(command: &mut Command) {",
        "unsafe {\n        command.pre_exec(lead_session_on_stdin);\n    }",
    ] {
        assert!(raw.contains(pin), "{pin}");
    }
    // ioctl(2) is exactly the five PTY requests, each at its one wrapper:
    // the request is never a parameter. setsid(2) is the pre-exec hook's
    // alone, and the hook is installed at one site.
    assert_eq!(raw.matches("SYS_IOCTL").count(), 6);
    for request in [
        "TIOCSPTLCK",
        "TIOCGPTPEER",
        "TIOCSWINSZ",
        "TIOCGWINSZ",
        "TIOCSCTTY",
    ] {
        assert_eq!(raw.matches(request).count(), 2, "{request}");
    }
    assert_eq!(raw.matches("SYS_SETSID").count(), 2);
    assert_eq!(raw.matches("pre_exec").count(), 1);
    assert_eq!(raw.matches("lead_session_on_stdin").count(), 2);
    assert_eq!(raw.matches("#[allow(unsafe_code)]").count(), 3);
    assert_eq!(raw.matches("PTY_PEER_FLAGS").count(), 2);
    assert_eq!(
        raw.matches("adopt(").count(),
        4,
        "the adoption site and its three callers"
    );
    assert!(!raw.contains("#![allow("));
    assert_eq!(raw.matches("core::arch::asm!").count(), 1);
    assert_eq!(raw.matches("from_raw_fd").count(), 1);
    // poll(2) is over exactly the two borrowed streams, readable only.
    assert_eq!(raw.matches("events: POLLIN,").count(), 2);
    assert_eq!(raw.matches("SYS_POLL").count(), 2);
    let production = raw.split("#[cfg(test)]").next().unwrap();
    assert!(!production.contains("pub fn"), "nothing raw is public");
    assert!(!production.contains("pub struct"));
    assert!(!production.contains("pub(crate) struct"));
    // The transport and the clipboard's destination owner are the only
    // callers, through exactly these wrappers and these call sites: the
    // owner's whole implementation, its doc comment through its drop, is
    // fingerprinted, and every status call in the module is one of the
    // owner's five, at the site and in the form pinned here.
    let clipboard = include_str!("../src/clipboard.rs");
    let clipboard = clipboard.split("#[cfg(test)]").next().unwrap();
    let owner = clipboard
        .split("/// The received write endpoint")
        .nth(1)
        .and_then(|rest| rest.split("/// An immutable UTF-8 text").next())
        .unwrap();
    let hash = owner.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    });
    assert_eq!(
        hash, 0xe1cbd12dd735cac3,
        "review the whole destination owner before updating its fingerprint"
    );
    assert_eq!(clipboard.matches("sys::").count(), 5);
    assert_eq!(owner.matches("sys::").count(), 5);
    assert_eq!(clipboard.matches("use crate::sys;").count(), 1);
    for site in [
        "let original = sys::status(&file)?;",
        "sys::set_status(file, original | O_NONBLOCK)?;",
        "if sys::status(file)? != original | O_NONBLOCK {",
        "sys::set_status(&file, self.original)?;",
        "if sys::status(&file)? != self.original {",
    ] {
        assert_eq!(owner.matches(site).count(), 1, "{site}");
    }
    assert_eq!(clipboard.matches("Destination::new(fd)?").count(), 1);
    assert_eq!(clipboard.matches("struct Destination {").count(), 1);
    assert_eq!(owner.matches("impl Drop for Destination {").count(), 1);
    assert!(!clipboard.contains("pub struct Destination"));
    assert!(!clipboard.contains("from_raw_fd") && !clipboard.contains("as_raw_fd"));
    assert!(owner.contains("if !kind.is_fifo() && !kind.is_socket() {"));
    assert!(clipboard.contains("const O_NONBLOCK: usize = 0o4000;"));
    assert!(clipboard.contains("const O_ACCMODE: usize = 3;"));
    // The client is the toolkit's one consumer of a received right, through
    // two pops: the keymap reader's, exactly one per `wl_keyboard.keymap`,
    // reading a regular file positionally and compiling it whole, and a
    // source's send, either board's, handed on typed or dropped (UNSAFE.md
    // §19).
    let client = include_str!("../src/client.rs");
    let client = client.split("#[cfg(test)]").next().unwrap();
    assert_eq!(
        client.matches(".pop_descriptor()").count(),
        3,
        "the consumer's pop, the keymap consumer and the send"
    );
    assert_eq!(
        client
            .matches("outcome(board, ClipboardEvent::Send(right))")
            .count(),
        1
    );
    assert_eq!(client.matches("ClipboardEvent::Send(").count(), 1);
    assert_eq!(client.matches("read_keymap(fd, format, size)").count(), 1);
    assert_eq!(client.matches("File::from(fd)").count(), 1);
    assert_eq!(client.matches("Keymap::parse(source)").count(), 1);
    assert!(client.contains("file.read_exact_at(&mut bytes, 0)"));
    assert!(client.contains("if size == 0 || size > 1024 * 1024 {"));
    assert!(client.contains("!metadata.is_file() || metadata.len() < u64::from(size)"));
    assert!(!client.contains("from_raw_fd") && !client.contains("as_raw_fd"));
    assert!(!client.contains("mmap"));
    // The PTY is the only caller of the four device ioctls and of the
    // session hook's installer, each at one site: the master is
    // opened without acquiring it and unlocked, its slave taken from it by
    // descriptor, every published size read back before it is trusted, and
    // a child made to lead a session on the slave exactly when it asks.
    let pty = include_str!("../src/pty.rs");
    let pty = pty.split("\n#[cfg(test)]\n#[allow(").next().unwrap();
    assert_eq!(pty.matches("sys::").count(), 5);
    assert_eq!(pty.matches("use crate::sys;").count(), 1);
    for call in [
        "sys::unlock_pty(&master)",
        "sys::pty_peer(&self.master)",
        "sys::set_window_size(&self.master, winsize_words(requested))",
        "sys::window_size(&terminal)",
        "    if command.leads_session {\n        sys::lead_session(&mut process);\n    }",
    ] {
        assert_eq!(pty.matches(call).count(), 1, "{call}");
    }
    assert!(pty.contains(".custom_flags(O_NOCTTY)"));
    assert!(pty.contains("pub const DEV_PTMX: &str = \"/dev/ptmx\";"));
    assert_eq!(pty.matches(".open(").count(), 1, "one device opened");
    assert!(pty.contains(".open(DEV_PTMX)"));
    assert!(pty.contains("const O_NOCTTY: i32 = 0o400;"));
    assert!(pty.contains("let observed = window_size(&self.master)?;"));
    assert!(!pty.contains("from_raw_fd") && !pty.contains("as_raw_fd"));
    assert!(
        pty.contains(".env_clear()"),
        "a child's environment is its caller's"
    );
    let transport = include_str!("../src/wayland.rs");
    assert_eq!(transport.matches("sys::").count(), 5);
    for call in [
        "sys::readable(&self.stream, own, millis)",
        "sys::inherited(fd)",
        "sys::send_file(&self.stream, suffix, right)",
        "sys::receive(&self.stream, &mut self.read)",
        "sys::receive(stream, &mut buffer)",
    ] {
        assert_eq!(transport.matches(call).count(), 1, "{call}");
    }
    assert_eq!(transport.matches("use crate::sys;").count(), 1);
    // The poll's one caller is the connection's wait.
    let read_more = transport
        .split("pub fn read_more(&mut self) -> Result<()> {")
        .nth(1)
        .and_then(|rest| rest.split("\n    pub fn ").next())
        .unwrap();
    assert!(read_more.contains("sys::readable(&self.stream, own, millis)"));
    assert_eq!(
        transport
            .matches("use super::{sys, wire, Connection, Message, Result, DESCRIPTORS, PENDING_BYTES, READ_BYTES};")
            .count(),
        1
    );
    assert!(!transport.contains("from_raw_fd"));
    assert!(
        !transport.contains("&mut VecDeque"),
        "the FIFO is never lent out"
    );
    let production = transport.split("#[cfg(test)]").next().unwrap();
    assert!(
        !production.contains("as_raw_fd"),
        "no raw number leaves sys"
    );
    for pin in [
        "pub const DESCRIPTORS: usize = 8;",
        "pub fn pop_descriptor(&mut self) -> Option<OwnedFd>",
        "pub fn descriptors(&self) -> usize",
        "if connection.descriptors.len() >= DESCRIPTORS {",
        "if bytes.len().saturating_add(count) > PENDING_BYTES",
        "|| files.len().saturating_add(fds.len()) > DESCRIPTORS",
        "pub const PENDING_BYTES: usize = 128 * 1024;",
        "pub const READ_BYTES: usize = 16 * 1024;",
        "pub const WRITE_DEADLINE: Duration = Duration::from_secs(5);",
        "pub const CONNECT_DEADLINE: Duration = Duration::from_secs(5);",
        ".create_new(true)",
        ".mode(0o600)",
        "std::fs::remove_file(&path)",
    ] {
        assert!(transport.contains(pin), "{pin}");
    }
}

#[test]
fn control_socket_publication_keeps_kernel_path_and_identity_checks_explicit() {
    let source = include_str!("../src/control_socket.rs");
    let production = source.split("#[cfg(test)]").next().unwrap();
    for pin in [
        "const O_DIRECTORY: i32 = 0o200000;",
        "const O_NOFOLLOW: i32 = 0o400000;",
        "const O_PATH: i32 = 0o10000000;",
        "const PATH_BYTES: usize = 107;",
        "const NAME_BYTES: usize = 80;",
        "const STATUS_BYTES: usize = 64 * 1024;",
        "UnixListener::bind(&pinned_path)",
        "custom_flags(O_PATH | O_DIRECTORY | O_NOFOLLOW)",
        "custom_flags(O_PATH | O_NOFOLLOW)",
        "File::open(\"/proc/self/status\")",
        "File::open(\"/proc/sys/kernel/overflowuid\")",
        "const UID_BYTES: usize = 11;",
        ".take(UID_BYTES as u64 + 1)",
        "unambiguous_uid(uid, overflow_uid(&bytes)?)",
        "overflow == uid || overflow == 0",
        "Permissions::from_mode(0o600)",
        "identity(&named) != identity(&self.node.metadata()?)",
        "fs::metadata(descriptor_path(&self.parent)).map_err",
        "control cleanup parent is unavailable",
        "trusted_ancestor(&current.metadata()?, uid)?",
        "metadata.uid() != uid && metadata.uid() != 0",
        "metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0",
    ] {
        assert!(production.contains(pin), "{pin}");
    }
    assert_eq!(production.matches("UnixListener::bind(").count(), 1);
    assert!(!production.contains("UnixStream::connect"));
    assert!(!production.contains("std::env"));
    assert!(!production.contains("env::"));
    assert!(!production.contains("canonicalize"));
    assert!(!production.contains("65534"));
    assert!(!production.contains("TD_TEST_TRUSTED_ROOT"));
    // The socket is the sole caller of the listener; the worker owns it
    // whole and the runner never opens one.
    for other in [
        include_str!("../src/control_worker.rs"),
        include_str!("../src/replay.rs"),
        include_str!("../src/control.rs"),
    ] {
        assert!(!other.contains("UnixListener"));
    }
}

#[test]
fn control_worker_keeps_bounded_nonblocking_transport_separate_from_consumer_state() {
    let source = include_str!("../src/control_worker.rs");
    let production = source.split("#[cfg(test)]").next().unwrap();
    for pin in [
        "pub const CONNECTIONS: usize = 8;",
        "const IO_BYTES: usize = 16 * 1024;",
        ".name(\"td-ui-control\".into())",
        "Duration::from_secs(5)",
        "Duration::from_millis(10)",
        "mpsc::sync_channel(CONNECTIONS)",
        "mpsc::sync_channel(1)",
        "for _ in connections.len()..CONNECTIONS",
        "requests.try_send(job)",
        "self.requests.try_recv()",
        "reply.try_send(response)",
        "R::parse(payload)",
        "thread::park_timeout(poll_interval(connections.is_empty()))",
        "Duration::from_millis(100)",
        ".join()",
    ] {
        assert!(production.contains(pin), "{pin}");
    }
    // The worker parses only through the consumer's `Parse` and touches no
    // consumer state: the one generic parameter is the request type.
    assert_eq!(production.matches("R::parse(").count(), 1);
    assert!(!production.contains("Controller"));
    assert!(!production.contains("App"));
    let socket = include_str!("../src/control_socket.rs");
    assert!(socket.contains("socket.listener.set_nonblocking(true)?"));
    assert!(socket.contains("stream.set_nonblocking(true)?"));
    for forbidden in [
        "mpsc::channel(",
        ".read_exact(",
        ".write_all(",
        ".recv()",
        ".send(",
    ] {
        assert!(!production.contains(forbidden), "{forbidden}");
    }
    // The replay runner is the framing's only other reader of a stream:
    // one header, one body, one framed reply, no state of its own.
    let replay = include_str!("../src/replay.rs");
    let replay = replay.split("#[cfg(test)]").next().unwrap();
    assert!(replay.contains("if length == 0 || length > MAX_FRAME {"));
    assert_eq!(replay.matches("input.read_exact(").count(), 2);
    assert_eq!(replay.matches("frame(reply.as_bytes())").count(), 1);
    assert!(!replay.contains("struct "));
    assert!(!replay.contains("static "));
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
        assert!(
            code.contains(&format!("{lint} = \"deny\"")),
            "{lint} denied"
        );
    }
    let lock =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock")).unwrap();
    assert_eq!(
        lock.lines()
            .filter(|line| line.trim() == "[[package]]")
            .count(),
        1,
        "the leaf's lock lists only itself"
    );
    assert!(!lock.contains("source ="));
}

/// The editor core's half of td-editor's caret-tick fence: a tick is
/// clock-checked and changes only the clock and the caret's visibility, so
/// a host may send it outside the input context fence.
#[test]
fn an_editor_tick_moves_only_the_clock_and_the_caret() {
    let ui = include_str!("../src/editor.rs");
    assert!(ui.contains(concat!(
        "Event::Tick(now) => {\n",
        "                if now < self.clock {\n",
        "                    return Err(Error::InvalidArgument);\n",
        "                }\n",
        "                let visible = self.focused && ((now - self.blink_start) / 500).is_multiple_of(2);\n",
        "                self.clock = now;\n",
        "                let changed = visible != self.caret_visible;\n",
        "                self.caret_visible = visible;\n",
        "                Ok(if changed {\n",
        "                    Outcome::Changed\n",
        "                } else {\n",
        "                    Outcome::Ignored\n",
        "                })\n",
        "            }"
    )));
}
