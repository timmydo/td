#![deny(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(20);
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "td-headless-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Process {
    child: Child,
    output: mpsc::Receiver<String>,
}

impl Process {
    fn start(command: &mut Command, log: &Path, marker: &'static str) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(fs::File::create(log).unwrap())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, output) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            for _ in 0..8 {
                let mut line = String::new();
                if reader.by_ref().take(4097).read_line(&mut line).is_err()
                    || line.is_empty()
                    || line.len() > 4096
                {
                    break;
                }
                if line.starts_with(marker) {
                    let _ = send.send(line);
                    break;
                }
            }
            // Keep stdout open and drained until process exit.
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        });
        Self { child, output }
    }

    fn ready(&self) -> String {
        self.output
            .recv_timeout(TIMEOUT)
            .expect("owned process did not become ready")
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "owned child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.child.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn headless(path: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_td-compositor"));
    command
        .args(["headless", "--session-dir"])
        .arg(path)
        .args(["--width", "800", "--height", "600"]);
    command
}

fn request(path: &Path, line: &[u8]) -> String {
    let mut stream = UnixStream::connect(path).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    stream.set_write_timeout(Some(TIMEOUT)).unwrap();
    stream.write_all(line).unwrap();
    let mut answer = String::new();
    stream.take(65537).read_to_string(&mut answer).unwrap();
    assert!(answer.len() <= 65536);
    answer
}

#[test]
fn keyboard_grant_routes_workspace_chords_across_one_shot_connections() {
    let root = Root::new();
    for enabled in [false, true] {
        let session = root.0.join(if enabled { "enabled" } else { "disabled" });
        let log = root.0.join(if enabled { "enabled.log" } else { "disabled.log" });
        let mut command = headless(&session);
        if enabled {
            command.args(["--input-control", "enabled"]);
        }
        let mut compositor = Process::start(&mut command, &log, "TD-COMPOSITOR-HEADLESS-READY");
        compositor.ready();
        let control = session.join("td-control");
        let expected = if enabled { "ok\n" } else { "error input automation is disabled\n" };
        for line in ["key 1 125 down\n", "key 2 3 down\n", "release-keys 3\n"] {
            assert_eq!(request(&control, line.as_bytes()), expected);
        }
        let layout = request(&control, b"layout\n");
        assert!(layout.contains(if enabled { "workspace active=2 " } else { "workspace active=1 " }));
        // Releasing Super must prevent a subsequent bare '1' from switching.
        assert_eq!(request(&control, b"key 4 2 down\n"), expected);
        assert_eq!(request(&control, b"release-keys 5\n"), expected);
        assert_eq!(request(&control, b"layout\n"), layout);
        assert!(request(&control, b"key 6 248 down\n").starts_with("error "));
        if enabled {
            assert_eq!(request(&control, b"key 7 125 down\n"), "ok\n");
            assert!(request(&control, b"key 8 20 down\n").starts_with("unavailable "));
            let mut command = Command::new(env!("CARGO_BIN_EXE_td-compositor"));
            command.arg0("td-ctl").arg("--socket").arg(&control)
                .args(["release-keys", "9"]);
            let mut ctl = Process::start(&mut command, &root.0.join("ctl.log"), "");
            // Successful order replies have no CLI stdout body.
            assert!(ctl.wait().success());
        }
        // EOF terminates the owning keyboard generation, even with held keys.
        assert_eq!(request(&control, b"key 10 42 down\n"), expected);
        compositor.child.stdin.take();
        assert!(compositor.wait().success(), "{}", fs::read_to_string(log).unwrap());
        assert!(!session.exists());
    }
}

#[test]
fn pointer_control_hits_real_workspace_chrome_with_a_mapped_native_client() {
    let root = Root::new();
    let session = root.0.join("pointer");
    let mut command = headless(&session);
    command.args(["--input-control", "enabled"]);
    let mut compositor = Process::start(
        &mut command, &root.0.join("compositor.log"), "TD-COMPOSITOR-HEADLESS-READY",
    );
    compositor.ready();
    let control = session.join("td-control");
    let mut command = Command::new(env!("CARGO_BIN_EXE_td-compositor"));
    command.arg0("td-ui-demo").args(["run", "--socket"])
        .arg(session.join("wayland-0")).arg("--ready-socket")
        .arg(root.0.join("client.ready"));
    let mut client = Process::start(&mut command, &root.0.join("client.log"), "TD-UI-CLIENT-READY");
    client.ready();
    assert_eq!(request(&control, b"workspace 2\n"), "ok\n");
    assert!(request(&control, b"layout\n").contains("visible=false focused=false"));
    // Workspace 1 holds the client and occupies the first top-bar cell.
    assert_eq!(request(&control, b"pointer 1 1 1 1 0 0\n"), "ok\n");
    assert_eq!(request(&control, b"release-input 2\n"), "ok\n");
    let layout = request(&control, b"layout\n");
    assert!(layout.contains("workspace active=1 "), "{layout}");
    assert!(layout.contains("visible=true focused=true"), "{layout}");
    for line in ["pointer 3 800 0 1 0 0\n", "pointer 3 0 600 1 0 0\n"] {
        assert_eq!(request(&control, line.as_bytes()), "error pointer coordinates outside the output\n");
    }
    assert_eq!(request(&control, b"layout\n"), layout);
    for line in [
        "pointer 4 300 300 1 0 0\n", "key 4 42 down\n",
        "release-keys 5\n", "pointer 6 310 310 1 1 -1\n", "release-input 7\n",
    ] {
        assert_eq!(request(&control, line.as_bytes()), "ok\n");
    }
    compositor.child.stdin.take();
    assert!(compositor.wait().success());
    assert!(!client.wait().success());
    assert!(!session.exists());
}

#[test]
fn a_disposable_production_session_maps_a_real_client_and_dies_with_its_owner() {
    let root = Root::new();
    let session = root.0.join("session");
    let log = root.0.join("compositor.log");
    let mut compositor = Process::start(
        &mut headless(&session),
        &log,
        "TD-COMPOSITOR-HEADLESS-READY",
    );
    assert_eq!(
        compositor.ready(),
        "TD-COMPOSITOR-HEADLESS-READY version=1 width=800 height=600 scale=1\n"
    );
    assert_eq!(
        fs::metadata(&session).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for name in ["wayland-0", "td-control"] {
        let metadata = fs::symlink_metadata(session.join(name)).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }
    assert_eq!(fs::read_dir(&session).unwrap().count(), 2);
    let control = session.join("td-control");
    let empty = request(&control, b"layout\n");
    assert!(empty.starts_with("ok\noutput "), "{empty}");
    assert!(empty.contains("windows=0\n"), "{empty}");

    let mut command = Command::new(env!("CARGO_BIN_EXE_td-compositor"));
    command
        .arg0("td-ui-demo")
        .args(["run", "--socket"])
        .arg(session.join("wayland-0"))
        .arg("--ready-socket")
        .arg(root.0.join("client.ready"));
    let mut demo = Process::start(
        &mut command,
        &root.0.join("client.log"),
        "TD-UI-CLIENT-READY",
    );
    assert!(demo.ready().starts_with("TD-UI-CLIENT-READY"));
    let mapped = request(&control, b"layout\n");
    assert!(mapped.contains("windows=1\n"), "{mapped}");
    assert!(mapped.contains("visible=true focused=true"), "{mapped}");
    // Real control orders still take the ordinary Runtime path.
    assert_eq!(request(&control, b"workspace 2\n"), "ok\n");
    let hidden = request(&control, b"layout\n");
    assert!(hidden.contains("visible=false focused=false"), "{hidden}");
    assert_eq!(request(&control, b"workspace 1\n"), "ok\n");
    compositor.child.stdin.take();
    assert!(
        compositor.wait().success(),
        "{}",
        fs::read_to_string(log).unwrap()
    );
    assert!(!session.exists());
    assert!(!demo.wait().success(), "disconnect should end the client");
}

#[test]
fn startup_refuses_existing_paths_and_rolls_back_a_failed_bind() {
    let root = Root::new();
    let session = root.0.join("occupied");
    fs::create_dir(&session).unwrap();
    fs::write(session.join("keep"), b"owned by caller").unwrap();
    let mut existing = Process::start(
        &mut headless(&session),
        &root.0.join("existing.log"),
        "TD-COMPOSITOR-HEADLESS-READY",
    );
    assert!(!existing.wait().success());
    assert_eq!(fs::read(session.join("keep")).unwrap(), b"owned by caller");
    let too_long = root.0.join("s".repeat(120));
    let mut refused = Process::start(
        &mut headless(&too_long),
        &root.0.join("long.log"),
        "TD-COMPOSITOR-HEADLESS-READY",
    );
    assert!(!refused.wait().success());
    assert!(!too_long.exists(), "failed startup left its new directory");
    assert!(refused.output.try_recv().is_err());
}

#[test]
fn lifetime_input_is_not_a_command_channel_and_client_files_are_not_removed() {
    let root = Root::new();
    let session = root.0.join("session");
    let mut compositor = Process::start(
        &mut headless(&session),
        &root.0.join("compositor.log"),
        "TD-COMPOSITOR-HEADLESS-READY",
    );
    compositor.ready();
    fs::write(session.join("keep"), b"client data").unwrap();
    compositor
        .child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"x")
        .unwrap();
    assert!(!compositor.wait().success());
    assert_eq!(fs::read(session.join("keep")).unwrap(), b"client data");
    assert!(!session.join("wayland-0").exists());
    assert!(!session.join("td-control").exists());
    let log = fs::read_to_string(root.0.join("compositor.log")).unwrap();
    assert_eq!(
        log.matches("remove headless session directory:").count(),
        1,
        "{log}"
    );
}

#[test]
fn shutdown_preserves_replaced_endpoint_and_directory_identities() {
    let root = Root::new();
    for replace_directory in [false, true] {
        let session = root.0.join(format!("session-{replace_directory}"));
        let mut compositor = Process::start(
            &mut headless(&session),
            &root.0.join(format!("{replace_directory}.log")),
            "TD-COMPOSITOR-HEADLESS-READY",
        );
        compositor.ready();
        let sentinel = if replace_directory {
            fs::rename(&session, root.0.join("original-session")).unwrap();
            fs::create_dir(&session).unwrap();
            session.join("keep")
        } else {
            let endpoint = session.join("wayland-0");
            fs::remove_file(&endpoint).unwrap();
            endpoint
        };
        fs::write(&sentinel, b"replacement belongs to caller").unwrap();
        compositor.child.stdin.take();
        assert!(!compositor.wait().success());
        assert_eq!(
            fs::read(sentinel).unwrap(),
            b"replacement belongs to caller"
        );
        if !replace_directory {
            assert!(
                !session.join("td-control").exists(),
                "replacement hid cleanup of another owned endpoint"
            );
        }
        let log = fs::read_to_string(root.0.join(format!("{replace_directory}.log"))).unwrap();
        assert_eq!(log.matches("td-compositor:").count(), 1, "{log}");
    }
}

#[test]
fn same_type_socket_replacement_is_not_the_owned_inode() {
    let root = Root::new();
    let session = root.0.join("session");
    let mut compositor = Process::start(
        &mut headless(&session),
        &root.0.join("compositor.log"),
        "TD-COMPOSITOR-HEADLESS-READY",
    );
    compositor.ready();
    let path = session.join("wayland-0");
    fs::remove_file(&path).unwrap();
    let replacement = UnixListener::bind(&path).unwrap();
    let identity = fs::symlink_metadata(&path).unwrap();
    compositor.child.stdin.take();
    assert!(!compositor.wait().success());
    let remaining = fs::symlink_metadata(&path).unwrap();
    assert_eq!(
        (remaining.dev(), remaining.ino()),
        (identity.dev(), identity.ino())
    );
    assert!(remaining.file_type().is_socket());
    assert!(!session.join("td-control").exists());
    drop(replacement);
}
