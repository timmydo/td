//! `td-util sha256sum`, run as a process: exit statuses, standard input,
//! options after operands, a list on standard input naming `-`, and a
//! reader that goes away mid-check.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

struct Ran {
    code: i32,
    out: String,
    err: String,
}

/// Runs `sha256sum ARGS` in `dir` with `input` on standard input, failing
/// rather than hanging past ten seconds.
fn sha256sum(dir: &Path, args: &[&OsStr], input: &[u8]) -> Ran {
    let mut child = Command::new(env!("CARGO_BIN_EXE_td-util"))
        .arg("sha256sum")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("sha256sum {args:?} did not finish");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    feeder.join().unwrap();
    let out = child.wait_with_output().unwrap();
    Ran {
        code: out.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&out.stdout).into_owned(),
        err: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn words<'a>(list: &[&'a str]) -> Vec<&'a OsStr> {
    list.iter().map(|word| OsStr::new(*word)).collect()
}

#[test]
fn no_operand_hashes_standard_input() {
    let dir = td_fs::TempDir::new("sha256sum-stdin").unwrap();
    let ran = sha256sum(&dir, &[], b"abc");
    assert_eq!(
        (ran.code, ran.out.as_str()),
        (0, format!("{ABC}  -\n").as_str())
    );
    let ran = sha256sum(&dir, &words(&["-b", "-"]), b"abc");
    assert_eq!(ran.out, format!("{ABC} *-\n"));
}

#[test]
fn an_unreadable_operand_fails_and_the_rest_are_still_hashed() {
    let dir = td_fs::TempDir::new("sha256sum-operands").unwrap();
    std::fs::write(dir.join("a"), b"abc").unwrap();
    let ran = sha256sum(&dir, &words(&["missing", "a"]), b"");
    assert_eq!(ran.code, 1);
    assert_eq!(ran.out, format!("{ABC}  a\n"));
    assert!(ran.err.starts_with("sha256sum: missing: "), "{}", ran.err);
}

#[test]
fn usage_help_and_version() {
    let dir = td_fs::TempDir::new("sha256sum-usage").unwrap();
    let ran = sha256sum(&dir, &words(&["-x"]), b"");
    assert_eq!(ran.code, 1);
    assert!(ran.err.contains("usage: sha256sum"), "{}", ran.err);
    for flag in ["--help", "--version"] {
        let ran = sha256sum(&dir, &words(&[flag]), b"");
        assert_eq!(ran.code, 0, "{flag}");
        assert!(ran.out.contains("sha256sum"), "{flag}");
    }
}

/// `-c LIST --status`, as installers spell it.
#[test]
fn options_after_the_list_apply() {
    let dir = td_fs::TempDir::new("sha256sum-permute").unwrap();
    std::fs::write(dir.join("a"), b"abc").unwrap();
    std::fs::write(dir.join("list"), format!("{ABC} a\n")).unwrap();
    let ran = sha256sum(&dir, &words(&["-c", "list", "--status"]), b"");
    assert_eq!((ran.code, ran.out.as_str(), ran.err.as_str()), (0, "", ""));
    let ran = sha256sum(&dir, &words(&["-c", "list"]), b"");
    assert_eq!((ran.code, ran.out.as_str()), (0, "a: OK\n"));
}

/// A list on standard input that names `-` would read standard input
/// twice; the line is improper and the rest are checked.
#[test]
fn a_list_on_stdin_naming_dash_does_not_hang() {
    let dir = td_fs::TempDir::new("sha256sum-dash").unwrap();
    std::fs::write(dir.join("a"), b"abc").unwrap();
    let list = format!("{ABC}  -\n{ABC}  a\n");
    for args in [&[][..], &["-"]] {
        let mut all = words(&["-c"]);
        all.extend(words(args));
        let ran = sha256sum(&dir, &all, list.as_bytes());
        assert_eq!((ran.code, ran.out.as_str()), (0, "a: OK\n"), "{args:?}");
        assert!(
            ran.err.contains("1 line is improperly formatted"),
            "{}",
            ran.err
        );
    }
}

/// td-util takes arguments as UTF-8; an operand that was not would name
/// another file, so it is refused.
#[test]
fn a_non_utf8_operand_is_refused() {
    let dir = td_fs::TempDir::new("sha256sum-lossy").unwrap();
    std::fs::write(dir.join("\u{fffd}\u{fffd}"), b"abc").unwrap();
    let ran = sha256sum(&dir, &[OsStr::from_bytes(b"\xff\xfe")], b"");
    assert_eq!((ran.code, ran.out.as_str()), (1, ""));
    assert!(ran.err.contains("not a UTF-8 name"), "{}", ran.err);
}

/// A check whose reader went away is not a success, whatever it printed.
#[test]
fn a_check_cut_short_by_its_reader_fails() {
    let dir = td_fs::TempDir::new("sha256sum-pipe").unwrap();
    std::fs::write(dir.join("a"), b"abc").unwrap();
    let mut list = String::new();
    for _ in 0..20_000 {
        list.push_str(&format!("{ABC}  a\n"));
    }
    std::fs::write(dir.join("list"), list).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_td-util"))
        .args(["sha256sum", "-c", "list"])
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    assert_eq!(child.wait().unwrap().code(), Some(1));
}
