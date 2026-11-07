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

use std::fs;
use std::os::unix::fs::PermissionsExt;
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

/// `token` as a whole identifier in `text`, not as part of a longer one.
fn has_token(text: &str, token: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(token).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(word)
            && !text[at + token.len()..].chars().next().is_some_and(word)
    })
}

/// The part of a file that ships: an inline test module may use what code may not.
fn production(text: &str) -> &str {
    text.split("#[cfg(test)]").next().unwrap_or_default()
}

/// Everything that runs in `td-sshd`, squeezed and without its test modules.
fn server_sources(tree: &Tree) -> Vec<(String, String)> {
    tree.rs
        .iter()
        .filter(|(name, _)| name.starts_with("server/") || name == "bin/td-sshd.rs")
        .map(|(name, text)| (name.clone(), squeeze(production(text))))
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

#[test]
fn sys_alone_holds_unsafe_with_exactly_its_two_allowances() {
    let tree = tree();
    for (name, text) in &tree.rs {
        let stripped = strip_comments(text);
        let (blocks, allows) = (unsafe_blocks(&stripped), unsafe_allows(&stripped));
        if name == "sys.rs" {
            assert_eq!((blocks, allows), (2, 2), "{name}");
        } else {
            assert_eq!((blocks, allows), (0, 0), "{name}");
        }
    }
    let lib = strip_comments(&source(&tree.rs, "lib.rs"));
    assert!(lib.contains("#![deny(unsafe_code)]"));
    for bin in ["bin/td-ssh.rs", "bin/td-ssh-keygen.rs", "bin/td-sshd.rs"] {
        let text = strip_comments(&source(&tree.rs, bin));
        assert!(text.contains("#![deny(unsafe_code)]"), "{bin}");
    }
}

#[test]
fn the_raw_layer_has_one_instruction_one_hook_and_no_escape_hatch() {
    let tree = tree();
    let sys = strip_comments(&source(&tree.rs, "sys.rs"));
    assert_eq!(sys.matches("asm!").count(), 1);
    assert_eq!(sys.matches(".pre_exec(").count(), 1);
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
    assert_eq!(
        constants(&sys, "SYS_"),
        pairs(&[
            ("SYS_IOCTL", "16"),
            ("SYS_UMASK", "95"),
            ("SYS_SETUID", "105"),
            ("SYS_SETGID", "106"),
            ("SYS_SETGROUPS", "116"),
            ("SYS_SETSID", "112"),
            ("SYS_PRCTL", "157"),
            ("SYS_SECCOMP", "317"),
        ])
    );
    assert_eq!(
        constants(&sys, "T"),
        pairs(&[
            ("TCGETS", "0x5401"),
            ("TCSETS", "0x5402"),
            ("TIOCGWINSZ", "0x5413"),
            ("TIOCSWINSZ", "0x5414"),
            ("TIOCSCTTY", "0x540e"),
            ("TIOCGPTN", "0x8004_5430"),
            ("TIOCSPTLCK", "0x4004_5431"),
        ])
    );
    assert_eq!(
        constants(&sys, "PR_"),
        pairs(&[("PR_SET_DUMPABLE", "4"), ("PR_SET_NO_NEW_PRIVS", "38")])
    );
    assert_eq!(
        constants(&sys, "SECCOMP_"),
        pairs(&[("SECCOMP_SET_MODE_FILTER", "1")])
    );
    assert_eq!(
        constants(&sys, "UMASK_"),
        pairs(&[("UMASK_PROBE", "0o077"), ("UMASK_FLOOR", "0o022")])
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
    assert_eq!(
        numbers,
        [
            "SYS_IOCTL",
            "SYS_IOCTL",
            "SYS_IOCTL",
            "SYS_IOCTL",
            "SYS_IOCTL",
            "SYS_IOCTL",
            "SYS_IOCTL",
            "SYS_PRCTL",
            "SYS_PRCTL",
            "SYS_SECCOMP",
            "SYS_SETGID",
            "SYS_SETGROUPS",
            "SYS_SETSID",
            "SYS_SETUID",
            "SYS_UMASK",
            "SYS_UMASK",
        ]
    );
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
    assert_eq!(
        requests,
        [
            "TCGETS",
            "TCSETS",
            "TIOCGPTN",
            "TIOCGWINSZ",
            "TIOCSCTTY",
            "TIOCSPTLCK",
            "TIOCSWINSZ",
        ]
    );
    // Each prctl site, naming one of the two pinned options.
    let mut options: Vec<&str> = squeezed
        .split("syscall3(SYS_PRCTL,")
        .skip(1)
        .map(|piece| piece.split(',').next().unwrap_or_default())
        .collect();
    options.sort_unstable();
    assert_eq!(options, ["PR_SET_DUMPABLE", "PR_SET_NO_NEW_PRIVS"]);
}

/// Every `SYS_` name anywhere. Only the raw layer has any to call with, and only
/// the jail's filter has others, which it lists and never calls.
#[test]
fn no_syscall_number_is_named_outside_the_raw_layer_and_the_filter() {
    let names = |text: &str| -> Vec<String> {
        let word = |c: char| c.is_alphanumeric() || c == '_';
        let mut found: Vec<String> = text
            .match_indices("SYS_")
            .filter(|(at, _)| !text[..*at].chars().next_back().is_some_and(word))
            .map(|(at, _)| text[at..].chars().take_while(|c| word(*c)).collect())
            .collect();
        found.sort_unstable();
        found.dedup();
        found
    };
    let tree = tree();
    for (name, text) in &tree.rs {
        let found = names(&strip_comments(text));
        match name.as_str() {
            "sys.rs" => assert_eq!(
                found,
                [
                    "SYS_IOCTL",
                    "SYS_PRCTL",
                    "SYS_SECCOMP",
                    "SYS_SETGID",
                    "SYS_SETGROUPS",
                    "SYS_SETSID",
                    "SYS_SETUID",
                    "SYS_UMASK",
                ]
            ),
            "server/jail.rs" => assert_eq!(
                found,
                [
                    "SYS_BRK",
                    "SYS_CLOCK_GETTIME",
                    "SYS_CLOCK_NANOSLEEP",
                    "SYS_CLOSE",
                    "SYS_EXIT",
                    "SYS_EXIT_GROUP",
                    "SYS_FUTEX",
                    "SYS_GETRANDOM",
                    "SYS_MADVISE",
                    "SYS_MMAP",
                    "SYS_MPROTECT",
                    "SYS_MREMAP",
                    "SYS_MUNMAP",
                    "SYS_READ",
                    "SYS_RT_SIGPROCMASK",
                    "SYS_RT_SIGRETURN",
                    "SYS_SCHED_YIELD",
                    "SYS_SIGALTSTACK",
                    "SYS_WRITE",
                ]
            ),
            _ => assert_eq!(found, [] as [&str; 0], "{name}"),
        }
    }
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
    // The listener loads a host key to check it, and holds nothing of it past that.
    let keys = squeeze(production(&strip_comments(&source(
        &tree.rs,
        "server/keys.rs",
    ))));
    let parsed = keys.find("PrivateKeyFile::parse(&text)").unwrap();
    assert!(parsed < keys.find("wipe_text(text);").unwrap());
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

/// `-V > /dev/full` must not exit 101.
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
    for args in [&["-V"][..], &["-F", "none", "-G", "h"]] {
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

/// Each line a td program runs `/bin/ssh-keygen` with, as it runs it: td-firstboot's
/// host key on every boot, td-vm-guest's git key and its proof, td-update's
/// fingerprint, and the generation bootsuccess, td-secret and td-vm-guest's
/// workspace use. Their own tests drive a stub or the host's OpenSSH, so this is the
/// only place the real tool meets them. A protected key under `-y -P ''` fails at
/// once, with no prompt on the closed stdin.
#[test]
fn every_line_a_td_program_runs_keygen_with_still_works() {
    let dir = td_fs::TempDir::new("td-ssh-callers").unwrap();
    let keygen = env!("CARGO_BIN_EXE_td-ssh-keygen");
    let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
    let (host, guest, plain) = (path("host"), path("guest"), path("plain"));
    let run = |args: &[&str], input: Option<&str>| {
        let mut command = Command::new(keygen);
        command.env_clear().current_dir("/").args(args);
        match input {
            Some(file) => command.stdin(fs::File::open(file).unwrap()),
            None => command.stdin(Stdio::null()),
        };
        let output = command.output().unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        output.stdout
    };
    let message = path("challenge");
    fs::write(&message, b"challenge\n").unwrap();
    run(
        &[
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "td-ssh-host-key",
            "-f",
            &host,
        ],
        None,
    );
    let public = run(&["-y", "-P", "", "-f", &host], None);
    assert!(public.starts_with(b"ssh-ed25519 "), "{public:?}");
    run(&["-l", "-E", "sha256", "-f", &format!("{host}.pub")], None);
    run(&["-l", "-f", &format!("{host}.pub")], None);
    run(
        &["-q", "-t", "ed25519", "-N", "", "-C", "td-vm", "-f", &guest],
        None,
    );
    let public = String::from_utf8(run(&["-y", "-P", "", "-f", &guest], None)).unwrap();
    run(&["-q", "-t", "ed25519", "-N", "", "-f", &plain], None);
    run(
        &[
            "-q",
            "-Y",
            "sign",
            "-n",
            "td-vm-key",
            "-f",
            &guest,
            &message,
        ],
        None,
    );
    let allowed = path("allowed");
    fs::write(&allowed, format!("td-vm {public}")).unwrap();
    run(
        &[
            "-q",
            "-Y",
            "verify",
            "-n",
            "td-vm-key",
            "-I",
            "td-vm",
            "-f",
            &allowed,
            "-s",
            &format!("{message}.sig"),
        ],
        Some(&message),
    );

    let protected = path("protected");
    fs::write(&protected, include_str!("fixtures/protected_ed25519")).unwrap();
    fs::set_permissions(&protected, fs::Permissions::from_mode(0o600)).unwrap();
    let output = Command::new(keygen)
        .env_clear()
        .args(["-y", "-P", "", "-f", &protected])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("Enter passphrase"),
        "{output:?}"
    );
    let output = Command::new(keygen)
        .env_clear()
        .args(["-y", "-P", "hunter2", "-f", &protected])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

/// The one print point filters, whatever reached it and from wherever.
#[test]
fn an_error_reaches_the_terminal_without_control_characters() {
    let output = Command::new(env!("CARGO_BIN_EXE_td-ssh"))
        .args(["-p", "x\x1b]0;PWNED\x07", "h"])
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
        .args(["-p", "nonsense", "h"])
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

/// Every call the raw layer makes that takes a value, whole: what each passes and
/// how many there are, which is what "value-pinned" and "one call site" mean.
#[test]
fn each_fixed_syscall_is_called_once_with_its_exact_arguments() {
    let tree = tree();
    let sys = squeeze(&strip_comments(&source(&tree.rs, "sys.rs")));
    for (number, count) in [
        ("SYS_SETGROUPS", 1),
        ("SYS_SETGID", 1),
        ("SYS_SETUID", 1),
        ("SYS_SETSID", 1),
        ("SYS_SECCOMP", 1),
        ("SYS_PRCTL", 2),
        ("SYS_UMASK", 2),
    ] {
        assert_eq!(
            sys.matches(&format!("syscall3({number},")).count(),
            count,
            "{number}"
        );
    }
    for exact in [
        "syscall3(SYS_SETGROUPS,list.len(),list.as_ptr()asusize,0)",
        "syscall3(SYS_SETGID,gidasusize,0,0)",
        "syscall3(SYS_SETUID,uidasusize,0,0)",
        // Dumpable is cleared, never set.
        "syscall3(SYS_PRCTL,PR_SET_DUMPABLE,0,0)",
        "syscall3(SYS_PRCTL,PR_SET_NO_NEW_PRIVS,1,0)",
        // No flags: nothing like TSYNC or a log-only mode can be asked for.
        "syscall3(SYS_SECCOMP,SECCOMP_SET_MODE_FILTER,0,std::ptr::from_ref(&program)asusize,)",
        "syscall3(SYS_UMASK,UMASK_PROBE,0,0)",
        // Only ever adds to the inherited mask.
        "syscall3(SYS_UMASK,inherited|UMASK_FLOOR,0,0)",
    ] {
        assert_eq!(sys.matches(exact).count(), 1, "{exact}");
    }
    let credentials = squeeze(&strip_comments(&source(&tree.rs, "server/credentials.rs")));
    for call in ["sys::setgroups(", "sys::setgid(", "sys::setuid("] {
        assert_eq!(credentials.matches(call).count(), 1, "{call}");
    }
}

/// A session and a controlling terminal are for an account child alone, in the
/// forked child before its exec: anywhere else they would move this process.
#[test]
fn setsid_and_tiocsctty_exist_only_inside_the_hook() {
    let tree = tree();
    let sys = squeeze(&strip_comments(&source(&tree.rs, "sys.rs")));
    let (before, rest) = sys.split_once(".pre_exec(").unwrap();
    let (hook, after) = rest.split_once("});").unwrap();
    assert_eq!(
        hook.matches("syscall3(SYS_SETSID,0,0,0)").count(),
        1,
        "{hook}"
    );
    assert_eq!(
        hook.matches("syscall3(SYS_IOCTL,0,TIOCSCTTY,0)").count(),
        1,
        "{hook}"
    );
    assert_eq!(hook.matches("syscall3(").count(), 2, "{hook}");
    for outside in [before, after] {
        for call in ["syscall3(SYS_SETSID", "TIOCSCTTY,"] {
            assert!(!outside.contains(call), "{call} outside the hook");
        }
    }
}

/// Who may call what. A new caller is an amendment, not a one-line change. A file
/// reaches a function if it names both the function and its module as identifiers,
/// however spelled: a path, `use module::{function}`, a rename or a re-export.
#[test]
fn each_wrapper_has_only_its_expected_callers() {
    let tree = tree();
    let rules: &[(&str, &str, &str, &[&str])] = &[
        ("sys.rs", "sys", "setgroups", &["server/credentials.rs"]),
        ("sys.rs", "sys", "setgid", &["server/credentials.rs"]),
        ("sys.rs", "sys", "setuid", &["server/credentials.rs"]),
        ("sys.rs", "sys", "set_no_new_privs", &["server/jail.rs"]),
        (
            "sys.rs",
            "sys",
            "install_seccomp_filter",
            &["server/jail.rs"],
        ),
        (
            "sys.rs",
            "sys",
            "set_not_dumpable",
            &["server/account.rs", "server/jail.rs", "server/session.rs"],
        ),
        ("sys.rs", "sys", "lead_session", &["server/session.rs"]),
        ("sys.rs", "sys", "open_pty", &["server/session.rs"]),
        ("sys.rs", "sys", "open_pty_slave", &["server/session.rs"]),
        ("sys.rs", "sys", "set_window_size", &["server/session.rs"]),
        (
            "sys.rs",
            "sys",
            "raise_umask_floor",
            &["server/listener.rs"],
        ),
        (
            "server/jail.rs",
            "jail",
            "enter",
            &["server/preauth.rs", "server/signer.rs"],
        ),
        (
            "server/credentials.rs",
            "credentials",
            "switch",
            &["server/account.rs", "server/jail.rs", "server/session.rs"],
        ),
    ];
    for (definer, module, function, allowed) in rules {
        let mut found: Vec<&str> = tree
            .rs
            .iter()
            .filter(|(name, _)| name != definer)
            .filter(|(_, text)| {
                let code = strip_comments(production(text));
                has_token(&code, module) && has_token(&code, function)
            })
            .map(|(name, _)| name.as_str())
            .collect();
        found.sort_unstable();
        let mut expected = allowed.to_vec();
        expected.sort_unstable();
        assert_eq!(found, expected, "{module}::{function}");
    }
    // The jail's own switch to the privsep account, by its direct import.
    let jail = strip_comments(&source(&tree.rs, "server/jail.rs"));
    assert_eq!(jail.matches("switch(").count(), 1);
}

/// The inventory above looks for a function under its module's name: a re-export
/// from an allowed file would give the rest a way in under another. There are none.
#[test]
fn nothing_is_re_exported() {
    for (name, text) in &tree().rs {
        for line in production(&strip_comments(text))
            .lines()
            .map(str::trim_start)
        {
            let visible = line.starts_with("pub")
                && line
                    .split_once(char::is_whitespace)
                    .is_some_and(|(_, rest)| rest.trim_start().starts_with("use "));
            assert!(!visible, "{name}: {line}");
        }
    }
}

/// `production` stops at the first `#[cfg(test)]`, so a line after one in mid-file
/// escapes every scan here. The first must open a module that the file ends with.
/// Sources are in rustfmt's style, so an item's closing brace is the first in column 0.
#[test]
fn in_every_source_the_first_test_attribute_opens_the_last_module() {
    for (name, text) in &tree().rs {
        let Some(at) = text.find("#[cfg(test)]") else {
            continue;
        };
        assert!(
            at == 0 || text[..at].ends_with('\n'),
            "{name}: the first #[cfg(test)] is not at the top level"
        );
        let mut lines = text[at..].lines().skip(1);
        let opening = lines.find(|line| !line.starts_with("#[")).unwrap_or("");
        assert!(
            opening.starts_with("mod ") && opening.ends_with(" {"),
            "{name}: the first #[cfg(test)] is on {opening:?}, not a module"
        );
        assert!(
            lines.any(|line| line == "}"),
            "{name}: {opening} is not closed"
        );
        assert!(
            lines.all(|line| line.trim().is_empty()),
            "{name}: something follows the test module"
        );
    }
}

/// `CommandExt::uid` and friends would change identity outside the one switch and
/// its readback. A read of `Metadata::uid()` takes no argument.
#[test]
fn no_server_stage_changes_identity_through_command() {
    for (name, text) in server_sources(&tree()) {
        for setter in [".uid(", ".gid(", ".groups("] {
            for (at, _) in text.match_indices(setter) {
                assert!(
                    text[at + setter.len()..].starts_with(')'),
                    "{name}: {setter}"
                );
            }
        }
        // `pre_exec` runs in the child, where no readback follows.
        assert!(!text.contains("pre_exec"), "{name}: pre_exec");
        // The same methods by any path, `Command::uid(&mut c, 0)` among them, and a
        // name for the trait to spell one with.
        for path in ["::uid", "::gid", "::groups"] {
            for (at, _) in text.match_indices(path) {
                let next = text[at + path.len()..].chars().next();
                assert!(
                    next.is_some_and(|c| c.is_alphanumeric() || c == '_'),
                    "{name}: {path}"
                );
            }
        }
        assert!(!text.contains("CommandExtas"), "{name}: CommandExt as");
    }
}

/// None of these fences shows in anything but a run as root, against an account
/// the host happens to have. So: the functions have tests, and this holds that the
/// paths that matter go through them.
#[test]
fn the_admission_fences_stand_where_they_are_needed() {
    let tree = tree();
    let code = |name: &str| squeeze(production(&strip_comments(&source(&tree.rs, name))));
    let account = code("server/account.rs");
    let resolve = account.split("pubfnresolve(").nth(1).unwrap();
    let resolve = resolve.split("pubfnfind_key(").next().unwrap();
    let uid_fence = "if!uid_may_log_in(account.uid){returnErr(";
    let secret_fence = "if!secret_admits(&db::secret(name)?){returnErr(";
    let fenced = resolve.find(uid_fence).expect(uid_fence);
    assert!(fenced < resolve.find(secret_fence).expect(secret_fence));
    // What is admitted past them is built after them. The single-user return comes
    // first, and is the caller's own uid, which `check_single_user` has seen.
    assert!(fenced < resolve.rfind("Ok(Account{").unwrap());
    let reader = account.split("fnparse_identity(").nth(1).unwrap();
    let reader = reader.split("fn").next().unwrap();
    assert!(
        reader.contains("if!uid_may_log_in(uid){returnNone;}"),
        "{reader}"
    );
    let main = account.split("pubfnread_user_file_main(").nth(1).unwrap();
    assert!(main.contains("parse_identity(identity)"));
    let sshd = code("bin/td-sshd.rs");
    let single = sshd.split("fnsingle_user(").nth(1).unwrap();
    let single = single.split("fnuser_of(").next().unwrap();
    assert!(single.contains("account::check_single_user(metadata.uid())?;"));
    let session = code("server/session.rs");
    let run = session.split("fnrun(single_user:bool)").nth(1).unwrap();
    let refused = run.find("refuse_root_grant(grant.uid)?;").unwrap();
    assert!(refused < run.find("switch(").unwrap(), "after the switch");
    assert!(refused < run.find("conn::spawn_reader(").unwrap());
}

/// The log's one door, which filters; and threads that cannot panic the stage when
/// none can be had. The reader's output goes down its stream, never to a stream
/// that a macro would panic on.
#[test]
fn server_code_uses_neither_the_panicking_print_macros_nor_thread_spawn() {
    let tree = tree();
    for (name, text) in server_sources(&tree) {
        for banned in [
            "eprintln!",
            "eprint!",
            "println!",
            "print!",
            "dbg!",
            "thread::spawn",
        ] {
            assert!(!text.contains(banned), "{name}: {banned}");
        }
    }
    let conn = squeeze(production(&source(&tree.rs, "conn.rs")));
    assert!(!conn.contains("thread::spawn"));
}

/// The key reader's whole contract with the account it becomes.
#[test]
fn the_key_reader_takes_its_identity_through_the_switch_and_is_bounded() {
    let tree = tree();
    let text = strip_comments(&source(&tree.rs, "server/account.rs"));
    let code = production(&text);
    let body = code.split("pub fn read_user_file_main(").nth(1).unwrap();
    assert_eq!(body.matches("read_checked(").count(), 1);
    let mut rest = body;
    for step in [
        "credentials::open_status(",
        "credentials::switch(",
        "sys::set_not_dumpable(",
        "authkeys::read_checked(",
        "write_all(",
    ] {
        let at = rest
            .find(step)
            .unwrap_or_else(|| panic!("{step} out of order"));
        rest = &rest[at + step.len()..];
    }
    let reader = code.split("fn collect_reader(").nth(1).unwrap();
    for needed in [
        ".stderr(Stdio::null())",
        "UntilDeadline::new(",
        "drop(command)",
    ] {
        assert!(reader.contains(needed), "{needed}");
    }
    assert!(!reader.contains(".wait()"), "an unbounded wait");
}

/// Fixed constants, and the bound that makes a stalled write end the connection,
/// which no test can wait out.
#[test]
fn the_session_arms_its_stall_limit_before_any_thread_starts() {
    let tree = tree();
    let text = squeeze(&strip_comments(&source(&tree.rs, "server/session.rs")));
    let run = text.split("fnrun(single_user:bool)").nth(1).unwrap();
    let armed = run
        .find("socket.set_write_timeout(Some(STALL_LIMIT))?")
        .unwrap();
    assert!(armed < run.find("conn::spawn_reader(").unwrap());
    // The call, not the definition further down. After the table, whose start the
    // first tick is measured from.
    let ticking = run.find("start_ticker(events)?").unwrap();
    assert!(run.find("Channels::new(").unwrap() < ticking);
}

/// Every way out of a client session: the farewell has its limit first, the terminal
/// is back before any line that is not raw-safe, and `~.` has the socket to end.
#[test]
fn the_client_ends_a_session_in_the_order_that_cannot_hang() {
    let tree = tree();
    let client = squeeze(production(&strip_comments(&source(&tree.rs, "client.rs"))));
    assert_eq!(client.matches("conn.disconnect(").count(), 1);
    let limit = client
        .find("control.set_write_timeout(Some(FAREWELL_WRITE))")
        .unwrap();
    assert!(limit < client.find("conn.disconnect(").unwrap());
    let finishing = client.find("finish_output(||{").unwrap();
    assert!(finishing < client.find("format!(\"rekeyed{}times\"").unwrap());
    assert!(client.contains("self.control.shutdown(Shutdown::Both)"));
    // Time at the host-key question is the person's, not the server's.
    let asked = client.find("letasked=Instant::now();").unwrap();
    assert!(asked < client.find("self.clock.extend(asked.elapsed());").unwrap());
    // Every session has a write limit, keepalive or not.
    assert!(client.contains("stream.set_write_timeout(Some(session_write_limit(options)))?"));
}

#[test]
fn the_credential_switch_keeps_its_order_and_reads_back() {
    let tree = tree();
    let text = strip_comments(&source(&tree.rs, "server/credentials.rs"));
    let body = production(&text).split("pub fn switch(").nth(1).unwrap();
    let order = [
        "read_status(status)",
        "may_switch(",
        "sys::setgroups(",
        "sys::setgid(",
        "sys::setuid(",
        "read_status(status)",
        "want.matches(&after)",
    ];
    let mut rest = body;
    for step in order {
        let at = rest
            .find(step)
            .unwrap_or_else(|| panic!("{step} out of order"));
        rest = &rest[at + step.len()..];
    }
}

#[test]
fn the_jail_applies_its_steps_in_the_one_safe_order() {
    let tree = tree();
    let text = strip_comments(&source(&tree.rs, "server/jail.rs"));
    let body = text.split("pub fn enter(").nth(1).unwrap();
    let order = [
        "check_jail_directory(",
        "chroot(",
        "set_current_dir(",
        "switch(",
        "sys::set_not_dumpable(",
        "drop(status)",
        "sys::set_no_new_privs(",
        "sys::install_seccomp_filter(",
    ];
    let mut rest = body;
    for step in order {
        let at = rest
            .find(step)
            .unwrap_or_else(|| panic!("{step} out of order"));
        rest = &rest[at + step.len()..];
    }
}

/// Stage processes are only spawned through std's `Command`, over stdio, with no
/// pre-exec hook on the path between them (DESIGN.md section 4).
#[test]
fn stages_pass_descriptors_by_stdio_alone() {
    let tree = tree();
    for name in [
        "server/account.rs",
        "server/listener.rs",
        "server/monitor.rs",
        "server/preauth.rs",
        "server/signer.rs",
    ] {
        let text = strip_comments(&source(&tree.rs, name));
        for banned in ["pre_exec", "SCM_RIGHTS", "sendmsg", "lead_session"] {
            assert!(!text.contains(banned), "{name}: {banned}");
        }
    }
}

/// The worker parses hostile bytes: it must hold no key and take its only input
/// by `File`, whose read and write are the syscalls its filter allows.
#[test]
fn the_pre_auth_worker_reads_no_key_and_uses_no_socket_type() {
    let tree = tree();
    let text = strip_comments(&source(&tree.rs, "server/preauth.rs"));
    for banned in [
        "PrivateKey",
        "load_host_key",
        "TcpStream",
        "UnixStream",
        "std::thread",
        "Command",
    ] {
        assert!(!text.contains(banned), "{banned}");
    }
}

/// Each `const NAME: u32 = VALUE;` of the jail, as a number.
fn jail_numbers(text: &str) -> Vec<(String, u32)> {
    text.lines()
        .filter_map(|line| line.strip_prefix("const "))
        .filter_map(|rest| {
            let (name, value) = rest.split_once(": u32 = ")?;
            let value = value.trim_end_matches(';').replace('_', "");
            let value = match value.strip_prefix("0x") {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => value.parse().ok()?,
            };
            Some((name.to_string(), value))
        })
        .collect()
}

/// One of the jail's `&[u32]` lists, resolved to the kernel's numbers.
fn jail_list(text: &str, name: &str, numbers: &[(String, u32)]) -> Vec<u32> {
    let opening = format!("const {name}: &[u32] = &[");
    let list = text.split(&opening).nth(1).unwrap();
    let list = list.split("];").next().unwrap();
    squeeze(list)
        .split(',')
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            numbers
                .iter()
                .find(|(constant, _)| constant == entry)
                .unwrap_or_else(|| panic!("{entry} has no number"))
                .1
        })
        .collect()
}

/// The jail's own tests compare the filter with these same constants, which a
/// changed number or a new entry would pass. Here are the numbers themselves.
#[test]
fn the_seccomp_allowlist_is_exactly_these_numbers() {
    let tree = tree();
    let text = strip_comments(&source(&tree.rs, "server/jail.rs"));
    let numbers = jail_numbers(&text);
    let allowed = jail_list(&text, "ALLOWED", &numbers);
    assert_eq!(
        allowed,
        [0, 1, 3, 11, 12, 14, 15, 24, 25, 28, 60, 131, 202, 228, 230, 231, 318]
    );
    let never_executable = jail_list(&text, "NEVER_EXECUTABLE", &numbers);
    assert_eq!(never_executable, [9, 10]);
    // No syscall number is defined and left out of the two lists.
    let defined: Vec<u32> = numbers
        .iter()
        .filter(|(name, _)| name.starts_with("SYS_"))
        .map(|(_, number)| *number)
        .collect();
    assert_eq!(defined.len(), allowed.len() + never_executable.len());
    for (name, value) in [
        ("AUDIT_ARCH_X86_64", 0xc000_003e),
        ("SECCOMP_RET_KILL_PROCESS", 0x8000_0000),
        ("SECCOMP_RET_ALLOW", 0x7fff_0000),
        ("X32_SYSCALL_BIT", 0x4000_0000),
        ("PROT_EXEC", 4),
        ("OFFSET_ARG2_LOW", 32),
        ("PRIVSEP_UID", 995),
        ("PRIVSEP_GID", 995),
    ] {
        let seen = text
            .lines()
            .find_map(|line| {
                let line = line.trim_start_matches("pub ");
                line.strip_prefix(&format!("const {name}: u32 = "))
            })
            .unwrap_or_else(|| panic!("{name} is not defined"));
        let digits = seen.trim_end_matches(';').replace('_', "");
        let number = match digits.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16).unwrap(),
            None => digits.parse().unwrap(),
        };
        assert_eq!(number, value, "{name}");
    }
}

#[test]
fn the_filter_names_no_dangerous_syscall() {
    let tree = tree();
    let text = strip_comments(&source(&tree.rs, "server/jail.rs"));
    for banned in [
        "SYS_OPEN",
        "SYS_OPENAT",
        "SYS_SOCKET",
        "SYS_CONNECT",
        "SYS_EXECVE",
        "SYS_CLONE",
        "SYS_FORK",
        "SYS_KILL",
        "SYS_TGKILL",
        "SYS_PTRACE",
        "SYS_SETUID",
    ] {
        assert!(!text.contains(banned), "{banned}");
    }
}
