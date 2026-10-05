//! `td-util xargs`, run as a process: batching, replacement, the empty-input
//! rule and the POSIX exit statuses.
#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::process::{Command, Stdio};

fn xargs(input: &str, args: &[&str]) -> (i32, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_td-util"))
        .arg("xargs")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn batches_echo_by_default_and_by_count() {
    assert_eq!(xargs("a b\nc\n", &[]), (0, "a b c\n".to_string()));
    assert_eq!(
        xargs("a b c", &["-n", "2", "echo", "x"]),
        (0, "x a b\nx c\n".to_string())
    );
    assert_eq!(xargs("a b c", &["-n1"]), (0, "a\nb\nc\n".to_string()));
    assert_eq!(
        xargs("a\0b c\0", &["-0", "echo"]),
        (0, "a b c\n".to_string())
    );
    assert_eq!(
        xargs("1 2 3 4", &["-s", "11", "echo"]),
        (0, "1 2 3\n4\n".to_string())
    );
}

#[test]
fn replace_runs_once_per_line() {
    assert_eq!(
        xargs("  one two\nthree\n", &["-I", "{}", "echo", "[{}]"]),
        (0, "[one two]\n[three]\n".to_string())
    );
    assert_eq!(
        xargs("p\nq\n", &["-I%", "echo", "%-%"]),
        (0, "p-p\nq-q\n".to_string())
    );
    // Quotes and backslashes are still read, blanks inside a line kept, and
    // blank lines skipped, as GNU does.
    assert_eq!(
        xargs(
            "'one  two'\na\\ b\n\nlead  trail  \n",
            &["-I{}", "echo", "[{}]"]
        ),
        (0, "[one  two]\n[a b]\n[lead  trail  ]\n".to_string())
    );
    assert_eq!(
        xargs("'open\n", &["-I{}", "echo", "{}"]),
        (1, String::new())
    );
}

#[test]
fn short_flags_bundle() {
    assert_eq!(
        xargs("a\0b\0", &["-0tn1", "echo"]),
        (0, "a\nb\n".to_string())
    );
    assert_eq!(xargs("", &["-r0", "echo", "ran"]), (0, String::new()));
    assert_eq!(
        xargs("p\n", &["-rI{}", "echo", "<{}>"]),
        (0, "<p>\n".to_string())
    );
    assert_eq!(xargs("", &["--bogus"]).0, 1);
    // An empty item between delimiters still runs under -I, as in GNU.
    assert_eq!(
        xargs("a\0\0b\0", &["-0", "-I{}", "echo", "[{}]"]),
        (0, "[a]\n[]\n[b]\n".to_string())
    );
}

#[test]
fn empty_input_runs_once_unless_r() {
    assert_eq!(xargs("", &["echo", "ran"]), (0, "ran\n".to_string()));
    assert_eq!(xargs("  \n", &["-r", "echo", "ran"]), (0, String::new()));
}

#[test]
fn exit_statuses_follow_posix() {
    assert_eq!(xargs("a", &["false"]).0, 123);
    assert_eq!(xargs("a", &["sh", "-c", "exit 255"]).0, 124);
    assert_eq!(xargs("a", &["sh", "-c", "kill -TERM $$"]).0, 125);
    assert_eq!(xargs("a", &["/nonexistent/td-util-xargs"]).0, 127);
    assert_eq!(xargs("'open", &["echo"]).0, 1);
}
