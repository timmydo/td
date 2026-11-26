//! Test-only peer that exits while its descendant still holds both output pipes.
#![cfg(test)]
#![allow(clippy::unwrap_used)]
use std::process::{Command, Stdio};

pub(crate) fn parent(line: &str, chatter: bool) -> Command {
    let mut command = Command::new("sh");
    // Hide libtest's banner but preserve stdout on fd 3 across both execs.
    command
        .args(["-c", r#""$0" --exact pipe_parent::spawn_child --nocapture 3>&1 >/dev/null; test "$?" = 42 && printf '%s\n' "$1""#])
        .arg(std::env::current_exe().unwrap())
        .arg(line)
        .env("TD_TEST_PIPE_CHILD", if chatter { "chatty" } else { "quiet" });
    command
}

#[test]
fn spawn_child() {
    let script = match std::env::var("TD_TEST_PIPE_CHILD").as_deref() {
        Ok("quiet") => "exec sleep 15",
        Ok("chatty") => "for i in 1 2 3 4 5 6 7 8 9 10; do echo x >&2; sleep 0.3; done",
        _ => return,
    };
    assert!(std::path::Path::new("/proc/self/fd/3").exists());
    // The fixture is the exiting parent. The child has a bounded lifetime.
    let _child = Command::new("sh")
        .args(["-c", script])
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    std::process::exit(42);
}
