//! Native integration uses the same deadline/receipt fixture as td-photo.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use td_ui::control::{frame, Decoder};

type Result<T> = std::result::Result<T, String>;
const TIMEOUT: Duration = Duration::from_secs(10);
static NEXT: AtomicU64 = AtomicU64::new(0);

#[path = "support/native_compositor.rs"]
mod native_compositor;

/// A short-lived private directory for a compositor session or a client's
/// runtime, at a Linux-socket-length path independent of TMPDIR.
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = Path::new("/tmp").join(format!(
            "td-taskmgr-process-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "native I/O deadline"))
}

fn write_until(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[path = "support/weston.rs"]
mod weston;

#[test]
fn relative_control_endpoint_is_refused_before_connecting_a_display() {
    let output = Command::new(env!("CARGO_BIN_EXE_td-taskmgr"))
        .args(["--control-socket", "relative-taskmgr-control"])
        .env_clear()
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("absolute"));
}

struct OwnedTarget(Child);
impl Drop for OwnedTarget {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl OwnedTarget {
    fn new() -> Self {
        use std::io::BufRead;
        let mut target = Self(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "owned_signal_target", "--ignored", "--nocapture"])
                .env("TD_TASKMGR_SIGNAL_TARGET", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let output = target.0.stdout.take().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(output).lines() {
                let line = line.unwrap();
                if line.starts_with("TD-OWNED ") {
                    let _ = send.send(line);
                    break;
                }
            }
        });
        assert_eq!(
            receive.recv_timeout(TIMEOUT).unwrap(),
            format!("TD-OWNED {}", target.0.id())
        );
        target
    }
    fn stopped(&self, expected: bool) {
        let until = Instant::now() + TIMEOUT;
        loop {
            let bytes = std::fs::read(format!("/proc/{}/stat", self.0.id())).unwrap();
            let observed = td_taskmgr::parsers::process(&bytes).unwrap();
            if matches!(observed.state, b'T' | b't') == expected {
                return;
            }
            assert!(
                Instant::now() < until,
                "owned target state {:?}",
                observed.state
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
#[test]
#[ignore = "owned child fixture"]
fn owned_signal_target() {
    if std::env::var("TD_TASKMGR_SIGNAL_TARGET").as_deref() != Ok("1") {
        return;
    }
    println!("TD-OWNED {}", std::process::id());
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}
fn exercise_controls(
    client: &native_compositor::TaskProcess,
    mut key: impl FnMut(&str),
    mut confirmation: impl FnMut(),
) {
    use native_compositor::{chord, wait_state};
    let target = OwnedTarget::new();
    wait_state(client, |s| s.get("actions").is_some_and(|s| s == "idle"));
    chord(client, "C-l");
    let query = target.0.id().to_string();
    chord(client, "C-f");
    chord(client, "C-a");
    chord(client, "BackSpace");
    for digit in query.chars() {
        chord(client, &digit.to_string());
    }
    chord(client, "Tab");
    chord(client, "Home");
    let until = Instant::now() + TIMEOUT;
    loop {
        let state = native_compositor::state(client);
        if state
            .get("selected")
            .is_some_and(|s| s.split(':').nth(1) == Some(query.as_str()))
        {
            break;
        }
        assert!(Instant::now() < until, "owned PID not selected: {state:?}");
        chord(client, "Down");
        std::thread::sleep(Duration::from_millis(20));
    }
    for (down, stopped) in [(2, true), (3, false)] {
        key("F10");
        wait_state(client, |s| s.get("actions").is_some_and(|s| s == "menu"));
        key("Right");
        for _ in 0..down {
            key("Down");
        }
        key("Return");
        wait_state(client, |s| {
            s.get("actions").is_some_and(|s| s == "confirmation")
        });
        confirmation();
        key("Tab");
        key("Return");
        wait_state(client, |s| s.get("actions").is_some_and(|s| s == "results"));
        target.stopped(stopped);
        key("Escape");
        wait_state(client, |s| s.get("actions").is_some_and(|s| s == "idle"));
    }
}
