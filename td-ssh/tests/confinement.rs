//! The source-level contract of UNSAFE.md section 27, which the compiler cannot
//! express: what the raw layer is made of, what it asks of the kernel, and which
//! modules may reach which part of it.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::path::Path;
use std::process::{Command, Stdio};
use td_source_scan::{
    read_tree, source, squeeze, strip_comments, unsafe_allows, unsafe_blocks, Tree,
};

fn tree() -> Tree {
    read_tree(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src")).unwrap()
}

/// Each `const NAME: usize = VALUE;` of the raw layer whose name starts `prefix`,
/// wherever it sits and whatever its visibility: one inside a function counts.
fn constants(text: &str, prefix: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let line = match line.strip_prefix("pub") {
                Some(rest) => rest.split_once("const ").map_or(rest, |(_, rest)| rest),
                None => line.strip_prefix("const ")?,
            };
            Some(line)
        })
        .filter(|rest| rest.starts_with(prefix))
        .filter_map(|rest| {
            let (name, value) = rest.split_once(':')?;
            let value = value.split_once('=')?.1.trim().trim_end_matches(';');
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

/// The part of a file that ships: an inline test module may use what code may not.
fn production(text: &str) -> &str {
    text.split("#[cfg(test)]").next().unwrap_or_default()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

#[test]
fn sys_alone_holds_unsafe_with_exactly_its_one_allowance() {
    let tree = tree();
    for (name, text) in &tree.rs {
        let stripped = strip_comments(text);
        let (blocks, allows) = (unsafe_blocks(&stripped), unsafe_allows(&stripped));
        if name == "sys.rs" {
            assert_eq!((blocks, allows), (1, 1), "{name}");
        } else {
            assert_eq!((blocks, allows), (0, 0), "{name}");
        }
    }
    let lib = strip_comments(&source(&tree.rs, "lib.rs"));
    assert!(lib.contains("#![deny(unsafe_code)]"));
    for bin in ["bin/td-ssh.rs", "bin/td-ssh-keygen.rs"] {
        let text = strip_comments(&source(&tree.rs, bin));
        assert!(text.contains("#![deny(unsafe_code)]"), "{bin}");
    }
}

#[test]
fn the_raw_layer_has_one_instruction_and_no_escape_hatch() {
    let tree = tree();
    let sys = strip_comments(&source(&tree.rs, "sys.rs"));
    assert_eq!(sys.matches("asm!").count(), 1);
    assert_eq!(sys.matches(".pre_exec(").count(), 0);
    assert_eq!(sys.matches("fn syscall3(").count(), 1);
    assert_eq!(sys.matches("pub fn syscall3").count(), 0);
    for banned in [
        "from_raw_fd",
        "FromRawFd",
        "mmap",
        "extern \"C\"",
        "transmute",
        "global_asm",
        "set_var",
        "remove_var",
        "*mut ",
    ] {
        assert!(!sys.contains(banned), "{banned}");
    }
}

#[test]
fn the_syscalls_and_requests_are_value_pinned() {
    let tree = tree();
    let sys = strip_comments(&source(&tree.rs, "sys.rs"));
    assert_eq!(constants(&sys, "SYS_"), pairs(&[("SYS_IOCTL", "16"),]));
    assert_eq!(
        constants(&sys, "T"),
        pairs(&[
            ("TCGETS", "0x5401"),
            ("TCSETS", "0x5402"),
            ("TIOCGWINSZ", "0x5413"),
        ])
    );
}

#[test]
fn every_syscall_call_site_names_its_number_and_request_literally() {
    let tree = tree();
    let sys = strip_comments(&source(&tree.rs, "sys.rs"));
    let mut calls: Vec<String> = Vec::new();
    for piece in sys.split("syscall3(").skip(1) {
        let args = squeeze(piece);
        calls.push(
            args.split(',')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string(),
        );
    }
    // The definition's own parameter list comes first.
    assert_eq!(calls.first().map(String::as_str), Some("number:usize"));
    // Exactly these, as many times each: a new call site is an amendment even when
    // its number is one of these, as is a number of its own under a `SYS_` name.
    let mut numbers: Vec<&str> = calls.iter().skip(1).map(String::as_str).collect();
    numbers.sort_unstable();
    assert_eq!(numbers, ["SYS_IOCTL", "SYS_IOCTL", "SYS_IOCTL"]);
    // Squeezed first: rustfmt breaks a long call after its opening parenthesis,
    // where a plain split would find no call site at all.
    let squeezed = squeeze(&sys);
    // Each ioctl site, naming its request as its third argument.
    let mut requests: Vec<&str> = squeezed
        .split("syscall3(SYS_IOCTL,")
        .skip(1)
        .map(|piece| piece.split(',').nth(1).unwrap_or_default())
        .collect();
    requests.sort_unstable();
    assert_eq!(requests, ["TCGETS", "TCSETS", "TIOCGWINSZ",]);
}

const CLIENT_SOURCES: &[&str] = &[
    "bin/td-ssh-keygen.rs",
    "bin/td-ssh.rs",
    "client.rs",
    "client_config.rs",
    "conn.rs",
    "error.rs",
    "kex.rs",
    "keyfile.rs",
    "keygen.rs",
    "known_hosts.rs",
    "lib.rs",
    "msg.rs",
    "packet.rs",
    "pattern.rs",
    "prompt.rs",
    "sys.rs",
    "wire.rs",
];

/// `println!` and friends panic on a failed write, which with `panic = "abort"`
/// and a raw terminal is the worst way to learn stderr was closed.
#[test]
fn client_output_goes_through_writes_that_can_fail() {
    let tree = tree();
    for name in CLIENT_SOURCES {
        let text = strip_comments(&source(&tree.rs, name));
        for banned in ["println!", "print!", "eprintln!", "eprint!", "dbg!"] {
            assert!(!text.contains(banned), "{name}: {banned}");
        }
    }
}

/// Raw mode goes on the descriptor that was tested. Only the prompt, which has
/// no such descriptor to ask, opens the controlling terminal by name.
#[test]
fn the_session_terminal_is_stdin_and_only_the_prompt_opens_dev_tty() {
    let tree = tree();
    for name in CLIENT_SOURCES {
        let text = strip_comments(&source(&tree.rs, name));
        assert_eq!(text.contains("/dev/tty"), *name == "prompt.rs", "{name}");
    }
    let client = squeeze(&strip_comments(&source(&tree.rs, "client.rs")));
    assert!(client.contains("sys::terminal_attributes(io::stdin())"));
    assert!(client.contains("sys::set_terminal_attributes(io::stdin(),&saved.raw())"));
    assert!(client.contains("sys::set_terminal_attributes(io::stdin(),&self.saved)"));
}

/// What cannot be read back after it is dropped is pinned by its shape: the types
/// holding KEX secrets wipe on drop, and identities do not outlive authentication.
#[test]
fn secrets_are_wiped_and_identities_dropped_after_authentication() {
    let tree = tree();
    let kex = strip_comments(&source(&tree.rs, "kex.rs"));
    for owner in [
        "Ephemeral",
        "Secret32",
        "SharedSecret",
        "DerivedKeys",
        "KexOutput",
    ] {
        assert!(kex.contains(&format!("impl Drop for {owner}")), "{owner}");
    }
    let keyfile = strip_comments(&source(&tree.rs, "keyfile.rs"));
    for owner in ["Wiped", "PrivateKeyFile"] {
        assert!(
            keyfile.contains(&format!("impl Drop for {owner}")),
            "{owner}"
        );
    }
    let client = squeeze(&strip_comments(&source(&tree.rs, "client.rs")));
    let run = client.split("pubfnrun(").nth(1).unwrap();
    let order = [
        "establish(stream,",
        "drop(identities);",
        // The call: the definition, further down, has `established:`.
        "session_loop(established,",
    ];
    let mut rest = run;
    for step in order {
        let at = rest
            .find(step)
            .unwrap_or_else(|| panic!("{step} out of order"));
        rest = &rest[at + step.len()..];
    }
    // The signing key cannot give its seed back, so the wiped copy is beside it.
    assert!(squeeze(&keyfile).contains("seed:Secret32,"));
}

/// The worker's chroot has no devices: a primitive that reads one itself would
/// work under test and fail in the jail (DESIGN.md section 7).
#[test]
fn randomness_comes_only_through_a_caller_held_descriptor() {
    for (name, text) in &tree().rs {
        let shipped = strip_comments(production(text));
        for reads_the_device in ["random::fill", "generate_secret", "generate_seed"] {
            assert!(!shipped.contains(reads_the_device), "{name}");
        }
    }
}

fn dev_full() -> std::fs::File {
    std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap()
}

/// `-Q kex > /dev/full` used to exit 101.
#[test]
fn a_full_stdout_is_an_ordinary_failure_status() {
    let run = |program: &str, args: &[&str]| {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(dev_full())
            .output()
            .unwrap()
    };
    for args in [&["-Q", "kex"][..], &["-V"], &["-F", "none", "-G", "h"]] {
        let output = run(env!("CARGO_BIN_EXE_td-ssh"), args);
        assert_eq!(output.status.code(), Some(255), "{args:?}: {output:?}");
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(!text.contains("panicked"), "{args:?}: {text}");
    }

    let dir = td_fs::TempDir::new("td-ssh-stdio").unwrap();
    let key = dir.path().join("k");
    let keygen = env!("CARGO_BIN_EXE_td-ssh-keygen");
    let key_text = key.to_str().unwrap();
    let generated = Command::new(keygen)
        .args(["-q", "-t", "ed25519", "-N", "", "-f", key_text])
        .output()
        .unwrap();
    assert!(generated.status.success(), "{generated:?}");
    for args in [&["-y", "-f", key_text][..], &["-l", "-f", key_text]] {
        let output = run(keygen, args);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(!text.contains("panicked"), "{args:?}: {text}");
    }
}

/// The one print point filters, whatever reached it and from wherever.
#[test]
fn an_error_reaches_the_terminal_without_control_characters() {
    let output = Command::new(env!("CARGO_BIN_EXE_td-ssh"))
        .args(["-Q", "x\x1b]0;PWNED\x07"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(255));
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("x?]0;PWNED?"), "{text}");
    assert!(!output.stderr.contains(&0x1b), "{text}");
}

#[test]
fn a_full_stderr_loses_the_message_and_keeps_the_status() {
    let status = Command::new(env!("CARGO_BIN_EXE_td-ssh"))
        .args(["-Q", "nonsense"])
        .stdin(Stdio::null())
        .stderr(dev_full())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(255));
    let status = Command::new(env!("CARGO_BIN_EXE_td-ssh-keygen"))
        .args(["-t", "rsa"])
        .stdin(Stdio::null())
        .stderr(dev_full())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1));
}
