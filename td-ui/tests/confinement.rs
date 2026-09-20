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

const PURE: [&str; 23] = [
    "charts.rs",
    "chrome.rs",
    "confirmations.rs",
    "control.rs",
    "data.rs",
    "driven.rs",
    "finder.rs",
    "keyboard.rs",
    "menus.rs",
    "pointer.rs",
    "raster.rs",
    "repeat.rs",
    "screen.rs",
    "split.rs",
    "tree_table.rs",
    "tree_table_geometry.rs",
    "tree_table_model.rs",
    "tree_table_paint.rs",
    "xkb.rs",
    "xkb_compat.rs",
    "xkb_keys.rs",
    "xkb_symbols.rs",
    "xkb_syntax.rs",
];

#[test]
fn source_inventory_and_shared_mounts_are_closed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(!root.join("build.rs").exists());
    let expected: BTreeSet<String> = PURE
        .iter()
        .chain(
            [
                "client.rs",
                "control_socket.rs",
                "control_worker.rs",
                "lib.rs",
                "notices.rs",
                "replay.rs",
                "screen_app.rs",
                "sys.rs",
                "wayland.rs",
            ]
            .iter(),
        )
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
                "sys.rs" => 4,
                _ => 0,
            },
            "unsafe keyword in {name}"
        );
        // The raw module is named by the crate root's private declaration
        // and by the transport's two imports and four wrapper calls; no
        // other module, shared source or test-support reader reaches it.
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
                "wayland.rs" => 6,
                _ => 0,
            },
            "raw-module access in {name}"
        );
        assert_eq!(
            compact.matches("#[path=").count(),
            if name == "lib.rs" { 4 } else { 0 },
            "source paths in {name}"
        );
        assert_eq!(
            text.matches(".pop_descriptor()").count(),
            match name.as_str() {
                "client.rs" => 3,
                "wayland.rs" => 1,
                _ => 0,
            },
            "pops: the transport's own test, the client's accessor, keymap reader and send: {name}"
        );
        for support in [".unconfigure(", ".input_mut("] {
            assert_eq!(
                text.matches(support).count(),
                0,
                "the client's test support {support} is not called in {name}"
            );
        }
        if name == "lib.rs" {
            assert!(compact.starts_with("#![deny(unsafe_code)]"));
            assert!(!compact.contains("#![allow("));
            assert!(compact.contains("modsys;"), "the raw module is declared");
            assert!(!compact.contains("pubmodsys"), "the raw module is private");
            assert!(compact.contains("pubmodwayland;"));
            for (file, declaration) in [
                ("filter.rs", "pubmodfilter;"),
                ("font.rs", "pubmodfont;"),
                ("font_data.rs", "modfont_data;"),
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
            // beside the face, and nothing else.
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
                ],
                "notices carry the three texts beside the face and nothing else"
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
        hash, 0xf41fcb5886e6e922,
        "review the complete raw layer before updating its fingerprint"
    );
    for pin in [
        "const SYS_SENDMSG: usize = 46;",
        "const SYS_RECVMSG: usize = 47;",
        "const SYS_FCNTL: usize = 72;",
        "const F_DUPFD_CLOEXEC: usize = 1030;",
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
        "#[allow(unsafe_code)]\nfn syscall5(",
        "#[allow(unsafe_code)]\nfn adopt(",
        "pub(crate) fn inherited(fd: i32) -> io::Result<UnixStream>",
        "pub(crate) fn receive(stream: &UnixStream, bytes: &mut [u8])",
        "pub(crate) fn send_file(stream: &UnixStream, bytes: &[u8], file: &File)",
    ] {
        assert!(raw.contains(pin), "{pin}");
    }
    assert!(!raw.contains("#![allow("));
    assert_eq!(raw.matches("core::arch::asm!").count(), 1);
    assert_eq!(raw.matches("from_raw_fd").count(), 1);
    let production = raw.split("#[cfg(test)]").next().unwrap();
    assert!(!production.contains("pub fn"), "nothing raw is public");
    assert!(!production.contains("pub struct"));
    assert!(!production.contains("pub(crate) struct"));
    // The transport is the only caller, through exactly these wrappers.
    // The client is the toolkit's one consumer of a received right, through
    // two pops: the keymap reader's, exactly one per `wl_keyboard.keymap`,
    // reading a regular file positionally and compiling it whole, and the
    // send's, handed on typed or dropped (UNSAFE.md §19).
    let client = include_str!("../src/client.rs");
    let client = client.split("#[cfg(test)]").next().unwrap();
    assert_eq!(
        client.matches(".pop_descriptor()").count(),
        3,
        "the consumer's pop, the keymap consumer and the send"
    );
    assert_eq!(
        client
            .matches("Handled::Clipboard(ClipboardEvent::Send(right))")
            .count(),
        1
    );
    assert_eq!(client.matches("read_keymap(fd, format, size)").count(), 1);
    assert_eq!(client.matches("File::from(fd)").count(), 1);
    assert_eq!(client.matches("Keymap::parse(source)").count(), 1);
    assert!(client.contains("file.read_exact_at(&mut bytes, 0)"));
    assert!(client.contains("if size == 0 || size > 1024 * 1024 {"));
    assert!(client.contains("!metadata.is_file() || metadata.len() < u64::from(size)"));
    assert!(!client.contains("from_raw_fd") && !client.contains("as_raw_fd"));
    assert!(!client.contains("mmap"));
    let transport = include_str!("../src/wayland.rs");
    assert_eq!(transport.matches("sys::").count(), 4);
    for call in [
        "sys::inherited(fd)",
        "sys::send_file(&self.stream, suffix, right)",
        "sys::receive(&self.stream, &mut self.read)",
        "sys::receive(stream, &mut buffer)",
    ] {
        assert_eq!(transport.matches(call).count(), 1, "{call}");
    }
    assert_eq!(transport.matches("use crate::sys;").count(), 1);
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
