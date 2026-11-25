#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! Source-level contracts the compiler cannot express: what the crate is
//! made of, that it depends on nothing, and the complete raw layer beneath
//! the PTY with its one caller (UNSAFE.md §29).

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

#[test]
fn source_inventory_is_closed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(!root.join("build.rs").exists());
    let expected: BTreeSet<String> = ["lib.rs", "queue.rs", "sys.rs"]
        .iter()
        .map(|name| name.to_string())
        .collect();
    let mut actual = BTreeSet::new();
    for entry in std::fs::read_dir(root.join("src")).unwrap() {
        let entry = entry.unwrap();
        assert!(
            entry.file_type().unwrap().is_file(),
            "no directory, symlinked or special source"
        );
        let name = entry.file_name().into_string().unwrap();
        let text = std::fs::read_to_string(entry.path()).unwrap();
        let compact = compact(&text);
        assert!(!compact.contains("include!("), "generated source in {name}");
        assert!(!compact.contains("#[path="), "source path in {name}");
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
        assert_eq!(
            text.matches(".take_for_test(").count(),
            0,
            "the queue's test support is not called in {name}"
        );
        // The raw module is named by the crate root's private declaration,
        // its four ioctl wrapper calls, the spawn's session hook and the
        // relay's six calls, and by the relay test's three reads; the queue
        // never reaches it.
        assert_eq!(
            identifier_count(&text, "sys"),
            match name.as_str() {
                "lib.rs" => 15,
                _ => 0,
            },
            "raw-module access in {name}"
        );
        if name == "lib.rs" {
            assert!(compact.starts_with("#![deny(unsafe_code)]"));
            assert!(!compact.contains("#![allow("));
            assert!(compact.contains("modsys;"), "the raw module is declared");
            assert!(!compact.contains("pubmodsys"), "the raw module is private");
            assert!(compact.contains("modqueue;"));
            assert!(compact.contains("pubusequeue::{InputQueue,MAX_INPUT_BYTES};"));
        }
        actual.insert(name);
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
        hash, 0x3d4869d9e21756f1,
        "review the complete raw layer before updating its fingerprint"
    );
    for pin in [
        "const SYS_IOCTL: usize = 16;",
        "const SYS_SETSID: usize = 112;",
        "const SYS_RT_SIGPROCMASK: usize = 14;",
        "const SYS_RT_SIGTIMEDWAIT: usize = 128;",
        "const TCGETS: usize = 0x5401;",
        "const TCSETS: usize = 0x5402;",
        "const RELAY_SIGNALS: u64 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 14) | (1 << 19);",
        "const TIOCSPTLCK: usize = 0x4004_5431;",
        "const TIOCGPTPEER: usize = 0x5441;",
        "const TIOCSWINSZ: usize = 0x5414;",
        "const TIOCGWINSZ: usize = 0x5413;",
        "const TIOCSCTTY: usize = 0x540e;",
        "const PTY_PEER_FLAGS: usize = 0o2 | 0o400 | 0o2_000_000;",
        "in(\"r10\") a4,",
        "in(\"r8\") a5,",
        "syscall5(number, a1, a2, a3, 0, 0)",
        "#[allow(unsafe_code)]\nfn syscall5(",
        "#[allow(unsafe_code)]\nfn adopt(",
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
        "pub(crate) fn terminal_attributes(terminal: &File) -> io::Result<[u8; 36]>",
        "pub(crate) fn set_terminal_attributes(terminal: &File, bytes: &[u8; 36]) -> io::Result<()>",
        "pub(crate) fn block_relay_signals() -> io::Result<u64>",
        "pub(crate) fn restore_relay_signals(previous: u64) -> io::Result<()>",
        "pub(crate) fn relay_signal() -> io::Result<Option<u8>>",
    ] {
        assert!(raw.contains(pin), "{pin}");
    }
    // ioctl(2) is exactly the five PTY and two termios requests, each at
    // its one wrapper: the request is never a parameter. setsid(2) is the
    // pre-exec hook's alone, and the hook is installed at one site.
    assert_eq!(raw.matches("SYS_IOCTL").count(), 8);
    for request in [
        "TIOCSPTLCK",
        "TIOCGPTPEER",
        "TIOCSWINSZ",
        "TIOCGWINSZ",
        "TIOCSCTTY",
        "TCGETS",
        "TCSETS",
    ] {
        assert_eq!(raw.matches(request).count(), 2, "{request}");
    }
    assert_eq!(raw.matches("SYS_SETSID").count(), 2);
    assert_eq!(raw.matches("SYS_RT_SIGPROCMASK").count(), 3);
    assert_eq!(raw.matches("SYS_RT_SIGTIMEDWAIT").count(), 2);
    assert_eq!(raw.matches("RELAY_SIGNALS").count(), 3);
    assert_eq!(raw.matches("pre_exec").count(), 1);
    assert_eq!(raw.matches("lead_session_on_stdin").count(), 2);
    assert_eq!(raw.matches("#[allow(unsafe_code)]").count(), 3);
    assert_eq!(raw.matches("PTY_PEER_FLAGS").count(), 2);
    assert_eq!(
        raw.matches("adopt(").count(),
        2,
        "the adoption site and its one caller"
    );
    assert!(!raw.contains("#![allow("));
    assert_eq!(raw.matches("core::arch::asm!").count(), 1);
    assert_eq!(raw.matches("from_raw_fd").count(), 1);
    for absent in ["SYS_POLL", "SYS_FCNTL", "SYS_SENDMSG", "SYS_RECVMSG"] {
        assert!(!raw.contains(absent), "{absent}");
    }
    assert!(!raw.contains("#[cfg(test)]"), "the raw layer has no tests");
    assert!(!raw.contains("pub fn"), "nothing raw is public");
    assert!(!raw.contains("pub struct"));
    assert!(!raw.contains("pub(crate) struct"));
    // The crate root is the only caller of the four device ioctls, the
    // session hook's installer and the relay's wrappers, each at one site:
    // the master is opened without acquiring it and unlocked, its slave
    // taken from it by descriptor, every published size read back before
    // it is trusted, and a child made to lead a session on the slave
    // exactly when it asks.
    let pty = include_str!("../src/lib.rs");
    let pty = pty.split("\n#[cfg(test)]\n#[allow(").next().unwrap();
    assert_eq!(pty.matches("sys::").count(), 11);
    assert_eq!(pty.matches("mod sys;").count(), 1);
    for call in [
        "sys::unlock_pty(&master)",
        "sys::pty_peer(&self.master)",
        "sys::set_window_size(&self.master, winsize_words(requested))",
        "sys::window_size(&terminal)",
        "    if command.leads_session {\n        sys::lead_session(&mut process);\n    }",
        "let original = sys::terminal_attributes(&terminal)?;",
        "let signals = sys::block_relay_signals()?;",
        "sys::set_terminal_attributes(&guard.terminal, &relay_attributes(original))?;",
        "sys::relay_signal()",
        "sys::set_terminal_attributes(&self.terminal, &self.original);",
        "sys::restore_relay_signals(previous)",
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
}

#[test]
fn the_crate_depends_on_nothing_and_declares_its_gate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let code: String = manifest
        .lines()
        .map(|line| line.split('#').next().unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!code.contains("dependencies]"), "std only");
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
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    assert_eq!(
        lock.lines()
            .filter(|line| line.trim() == "[[package]]")
            .count(),
        1,
        "the lock lists only td-pty"
    );
    assert!(!lock.contains("source ="));
}
