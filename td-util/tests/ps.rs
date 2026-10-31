#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

//! Process queries use an ordinary child, with no application fixtures.
use std::process::{Child, Command, Output, Stdio};

struct Sleeper(Child);
impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn ps(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_td-util"))
        .arg("ps")
        .args(args)
        .env("TZ", "/nonexistent-td-util-ps-zone")
        .output()
        .unwrap()
}

#[test]
fn selected_child_has_stable_start_time_and_disappears_after_reaping() {
    let mut child = Sleeper(
        Command::new(env!("CARGO_BIN_EXE_td-util"))
            .args(["sleep", "60"])
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = child.0.id().to_string();
    let query = || ps(&["-p", &pid, "-o", "pid=", "-o", "stat=", "-o", "lstart="]);
    let first = query();
    assert!(first.status.success(), "{:?}", first);
    let text = String::from_utf8(first.stdout).unwrap();
    let fields: Vec<_> = text.split_whitespace().collect();
    assert_eq!(fields.len(), 7, "{text}");
    assert_eq!(fields[0], pid);
    assert!(fields[1].starts_with(['R', 'S']));
    let date = fields[2..].join(" ");
    let second = query();
    assert!(second.status.success());
    let second = String::from_utf8(second.stdout).unwrap();
    assert_eq!(
        second
            .split_whitespace()
            .skip(2)
            .collect::<Vec<_>>()
            .join(" "),
        date
    );
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let gone = query();
    assert_eq!(gone.status.code(), Some(1));
    assert!(gone.stdout.is_empty());
}

#[test]
fn output_headers_and_invalid_arguments() {
    let pid = std::process::id().to_string();
    let output = ps(&["-p", &pid, "-o", "pid,lstart"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(text.lines().count(), 2);
    assert!(text.starts_with("PID STARTED\n"));
    for args in [
        vec!["-p"],
        vec!["-p", "0"],
        vec!["-p", "wat"],
        vec!["-o", "unknown"],
        vec!["-o"],
    ] {
        let output = ps(&args);
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty());
    }
}
