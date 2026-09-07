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
    fn to_file(command: &mut Command, log: &Path, output: &Path) -> Self {
        let child = command.stdin(Stdio::null())
            .stdout(fs::File::create(output).unwrap())
            .stderr(fs::File::create(log).unwrap()).spawn().unwrap();
        let (_send, output) = mpsc::channel();
        Self { child, output }
    }

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

fn input_request(path: &Path, session: &str, line: &str) -> String {
    let (verb, rest) = line.split_once(' ').unwrap();
    request(path, format!("{verb} {session} {rest}").as_bytes())
}

fn input_receipt(session: &str, action: u64) -> String {
    format!("ok\ntd-action-v1 session={session} action={action}\n")
}

fn capture(path: &Path, root: &Path, name: &str) -> (ExitStatus, Vec<u8>) {
    query_cli(path, root, name, "capture")
}

fn query_cli(path: &Path, root: &Path, name: &str, verb: &str) -> (ExitStatus, Vec<u8>) {
    query_cli_args(path, root, name, &[verb])
}

fn query_cli_args(path: &Path, root: &Path, name: &str, args: &[&str]) -> (ExitStatus, Vec<u8>) {
    let verb = args.first().unwrap();
    let extension = if *verb == "capture" { "ppm" } else { "out" };
    let output = root.join(format!("{name}.{extension}"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_td-compositor"));
    command.arg0("td-ctl").arg("--socket").arg(path).args(args);
    let mut client = Process::to_file(&mut command, &root.join(format!("{name}.log")), &output);
    let status = client.wait();
    (status, fs::read(output).unwrap())
}

fn ready_session(line: &str) -> &str {
    let session = line.strip_prefix("TD-COMPOSITOR-HEADLESS-READY version=2 session=").unwrap()
        .strip_suffix(" width=800 height=600 scale=1\n").unwrap();
    assert_eq!(session.len(), 32);
    assert!(session.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    session
}

fn captured_output(ppm: &[u8]) -> (&str, u64, &[u8]) {
    let mut lines = ppm.splitn(5, |b| *b == b'\n');
    assert_eq!(lines.next(), Some(b"P6".as_slice()));
    let stamp = std::str::from_utf8(lines.next().unwrap()).unwrap()
        .strip_prefix("# td-output-v1 session=").unwrap();
    let (session, output) = stamp.split_once(" output=").unwrap();
    assert_eq!(lines.next(), Some(b"800 600".as_slice()));
    assert_eq!(lines.next(), Some(b"255".as_slice()));
    let pixels = lines.next().unwrap();
    assert_eq!(pixels.len(), 800 * 600 * 3);
    (session, output.parse().unwrap(), pixels)
}

#[test]
fn native_client_publications_correlate_routed_input_with_captured_output() {
    let root = Root::new();
    let session = root.0.join("session");
    let mut command = headless(&session);
    command.args(["--input-control", "enabled", "--capture-control", "enabled"]);
    let mut compositor = Process::start(
        &mut command, &root.0.join("compositor.log"), "TD-COMPOSITOR-HEADLESS-READY",
    );
    let ready = compositor.ready();
    let identity = ready_session(&ready);
    let control = session.join("td-control");
    let mut command = Command::new(env!("CARGO_BIN_EXE_td-compositor"));
    command.arg0("td-ui-demo").args(["run", "--socket"])
        .arg(session.join("wayland-0")).arg("--ready-socket")
        .arg(root.0.join("client.ready"));
    let mut client = Process::start(
        &mut command, &root.0.join("client.log"), "TD-UI-CLIENT-READY",
    );
    client.ready();
    let layout = request(&control, b"layout\n");
    let window = layout.lines().find_map(|line| line.strip_prefix("window id="))
        .unwrap().split_whitespace().next().unwrap();
    let decode = |reply: &str| {
        let prefix = format!("td-client-v1 session={identity} window={window} client=");
        let body = reply.strip_prefix(&prefix).unwrap();
        let (client, body) = body.split_once(" commit=").unwrap();
        let (commit, body) = body.split_once(" output=").unwrap();
        let output = body.strip_suffix(" current=yes\n").unwrap();
        (client.parse::<u64>().unwrap(), commit.parse::<u64>().unwrap(),
            output.parse::<u64>().unwrap())
    };
    let observe = || {
        let reply = request(&control, format!("observe-client {identity} {window}\n").as_bytes());
        decode(reply.strip_prefix("ok\n").unwrap())
    };
    let (client_id, first_commit, first_output) = observe();
    assert!(client_id > 0 && first_commit >= 2 && first_output > 0);
    let (status, cli) = query_cli_args(&control, &root.0, "client-observe",
        &["observe-client", identity, window]);
    assert!(status.success());
    let (cli_client, cli_commit, cli_output) = decode(std::str::from_utf8(&cli).unwrap());
    assert_eq!(cli_client, client_id);
    assert!(cli_commit >= first_commit && cli_output >= first_output);
    // Readiness does not order the independent seat worker's focus events.
    let deadline = Instant::now() + TIMEOUT;
    let initial_commit = loop {
        assert!(Instant::now() < deadline, "initial client publication did not settle");
        let (observed_client, commit, output) = observe();
        assert_eq!(observed_client, client_id);
        let (status, before) = capture(&control, &root.0, "before");
        assert!(status.success());
        let (final_client, final_commit, final_output) = observe();
        assert_eq!(final_client, client_id);
        if commit != final_commit {
            continue;
        }
        let (captured_session, captured, pixels) = captured_output(&before);
        assert_eq!(captured_session, identity);
        assert!(captured > output && captured <= final_output);
        assert!(!demo_a_up_visible(pixels));
        break commit;
    };
    assert_eq!(input_request(&control, identity, "key 1 30 down\n"), input_receipt(identity, 1));
    assert_eq!(input_request(&control, identity, "key 2 30 up\n"), input_receipt(identity, 2));
    let deadline = Instant::now() + TIMEOUT;
    loop {
        assert!(Instant::now() < deadline, "native input did not produce correlated client output");
        let (observed_client, commit, output) = observe();
        assert_eq!(observed_client, client_id);
        if commit <= initial_commit {
            continue;
        }
        let (status, after) = capture(&control, &root.0, "after");
        assert!(status.success());
        let (final_client, final_commit, final_output) = observe();
        assert_eq!(final_client, client_id);
        if commit != final_commit {
            continue;
        }
        let (captured_session, captured, pixels) = captured_output(&after);
        assert_eq!(captured_session, identity);
        assert!(captured > output && captured <= final_output);
        if !demo_a_up_visible(pixels) {
            continue;
        }
        break;
    }
    let stale = if identity == "00000000000000000000000000000000" {
        "00000000000000000000000000000001"
    } else {
        "00000000000000000000000000000000"
    };
    let (status, body) = query_cli_args(&control, &root.0, "stale-observe",
        &["observe-client", stale, window]);
    assert_eq!(status.code(), Some(1));
    assert!(body.is_empty());
    client.child.kill().unwrap();
    assert!(!client.wait().success());
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let reply = request(&control, format!("observe-client {identity} {window}\n").as_bytes());
        if reply == "unavailable client observation window is gone\n" {
            break;
        }
        assert!(Instant::now() < deadline, "disconnected client observation remained live");
    }
    compositor.child.stdin.take();
    assert!(compositor.wait().success());
}

fn demo_a_up_visible(pixels: &[u8]) -> bool {
    // Independent golden 5x7 glyphs for the demo's "A UP" suffix at scale 2.
    // Focus/geometry repaint alone must not satisfy input completion.
    let glyphs = [
        [14u8, 17, 17, 31, 17, 17, 17],
        [0; 7],
        [17, 17, 17, 17, 17, 17, 14],
        [30, 17, 17, 30, 16, 16, 16],
    ];
    let yellow = |x: usize, y: usize| {
        pixels.get((y * 800 + x) * 3..(y * 800 + x) * 3 + 3) == Some(&[0xff, 0xe8, 0xc0])
    };
    for y in 0..=600 - 14 {
        for x in 0..=800 - 48 {
            if !yellow(x + 2, y) {
                continue;
            }
            let matches = (0..14).all(|dy| (0..48).all(|dx| {
                let column = (dx / 2) % 6;
                let row = glyphs.get(dx / 12).and_then(|glyph| glyph.get(dy / 2)).unwrap();
                let ink = column < 5 && row & (1 << (4 - column)) != 0;
                yellow(x + dx, y + dy) == ink
            }));
            if matches {
                return true;
            }
        }
    }
    false
}

#[test]
fn reusing_a_session_path_starts_a_new_capture_identity() {
    let root = Root::new();
    let session = root.0.join("session");
    let mut previous = None;
    for generation in 0..2 {
        let mut command = headless(&session);
        command.args(["--capture-control", "enabled", "--input-control", "enabled"]);
        let mut compositor = Process::start(
            &mut command, &root.0.join(format!("compositor-{generation}.log")),
            "TD-COMPOSITOR-HEADLESS-READY",
        );
        let ready = compositor.ready();
        let identity = ready_session(&ready);
        let (status, ppm) = capture(&session.join("td-control"), &root.0,
            &format!("capture-{generation}"));
        assert!(status.success());
        let (captured_session, output, _) = captured_output(&ppm);
        assert_eq!(captured_session, identity);
        assert_eq!(output, 2, "a new runtime inherited old output numbering");
        assert_ne!(previous.as_deref(), Some(identity), "a restarted session reused its nonce");
        if let Some(old_session) = previous.as_deref() {
            let control = session.join("td-control");
            let before = request(&control, b"layout\n");
            for line in ["key 1 125 down\n", "key 2 3 down\n", "pointer 3 1 1 1 0 0\n",
                "release-keys 4\n", "release-input 5\n"]
            {
                assert_eq!(input_request(&control, old_session, line),
                    "error input session identity does not match\n");
            }
            assert_eq!(request(&control, b"layout\n"), before);
        }
        assert_eq!(input_request(&session.join("td-control"), identity, "release-input 6\n"),
            input_receipt(identity, 1));
        previous = Some(identity.to_string());
        compositor.child.stdin.take();
        assert!(compositor.wait().success());
        assert!(!session.exists());
    }
}

#[test]
fn binary_capture_cli_requires_its_own_grant_and_captures_real_client_output() {
    let root = Root::new();
    for (input, capture_enabled) in [(false, false), (true, false), (false, true), (true, true)] {
        let case = root.0.join(format!("case-{input}-{capture_enabled}"));
        fs::create_dir(&case).unwrap();
        let session = root.0.join(format!("session-{input}-{capture_enabled}"));
        let mut command = headless(&session);
        if input {
            command.args(["--input-control", "enabled"]);
        }
        if capture_enabled {
            command.args(["--capture-control", "enabled"]);
        }
        let mut compositor = Process::start(
            &mut command, &case.join("compositor.log"), "TD-COMPOSITOR-HEADLESS-READY",
        );
        let ready = compositor.ready();
        let identity = ready_session(&ready);
        let control = session.join("td-control");
        if capture_enabled {
            let expected = format!("ok\ntd-output-v1 session={identity} output=1 current=yes\n");
            assert_eq!(request(&control, b"observe\n"), expected);
            assert_eq!(request(&control, b"observe\n"), expected, "observe caused a paint");
        } else {
            assert_eq!(request(&control, b"observe\n"), "error capture automation is disabled\n");
            assert_eq!(request(&control, format!("observe-client {identity} @1\n").as_bytes()),
                "error capture automation is disabled\n");
        }
        let (status, empty) = capture(&control, &case, "empty");
        let mut mapped_client = None;
        if !capture_enabled {
            assert_eq!(status.code(), Some(2));
            assert!(empty.is_empty());
        } else {
            assert!(status.success());
            let (session_stamp, output_stamp, empty_pixels) = captured_output(&empty);
            assert_eq!(session_stamp, identity);
            assert_eq!(output_stamp, 2);
            let (status, observed) = query_cli(&control, &case, "observe", "observe");
            assert!(status.success());
            assert_eq!(observed, format!(
                "td-output-v1 session={identity} output=2 current=yes\n",
            ).as_bytes());
            let mut command = Command::new(env!("CARGO_BIN_EXE_td-compositor"));
            command.arg0("td-ui-demo").args(["run", "--socket"])
                .arg(session.join("wayland-0")).arg("--ready-socket")
                .arg(case.join("client.ready"));
            let client = Process::start(
                &mut command, &case.join("client.log"), "TD-UI-CLIENT-READY",
            );
            client.ready();
            let (status, mapped) = capture(&control, &case, "mapped");
            assert!(status.success());
            let (session_stamp, mapped_stamp, mapped_pixels) = captured_output(&mapped);
            assert_eq!(session_stamp, identity);
            assert!(mapped_stamp > output_stamp);
            assert_ne!(mapped_pixels, empty_pixels, "capture did not contain the mapped client");
            assert_eq!(request(&control, b"workspace 2\n"), "ok\n");
            let (status, hidden) = capture(&control, &case, "hidden");
            assert!(status.success());
            let (session_stamp, hidden_stamp, hidden_pixels) = captured_output(&hidden);
            assert_eq!(session_stamp, identity);
            assert!(hidden_stamp > mapped_stamp);
            assert_ne!(hidden_pixels, mapped_pixels, "capture returned stale visible-client output");
            assert!(request(&control, b"capture extra\n").starts_with("error "));
            if !input {
                assert_eq!(input_request(&control, identity, "key 1 30 down\n"),
                    "error input automation is disabled\n");
            }
            mapped_client = Some(client);
        }
        compositor.child.stdin.take();
        assert!(compositor.wait().success());
        if let Some(mut client) = mapped_client {
            assert!(!client.wait().success());
        }
        assert!(!session.exists());
    }
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
        let ready = compositor.ready();
        let identity = ready_session(&ready);
        let control = session.join("td-control");
        let expected = |action| if enabled { input_receipt(identity, action) }
            else { "error input automation is disabled\n".into() };
        for (index, line) in ["key 1 125 down\n", "key 2 3 down\n", "release-keys 3\n"]
            .into_iter().enumerate()
        {
            assert_eq!(input_request(&control, identity, line), expected(index as u64 + 1));
        }
        let layout = request(&control, b"layout\n");
        assert!(layout.contains(if enabled { "workspace active=2 " } else { "workspace active=1 " }));
        // Releasing Super must prevent a subsequent bare '1' from switching.
        assert_eq!(input_request(&control, identity, "key 4 2 down\n"), expected(4));
        assert_eq!(input_request(&control, identity, "release-keys 5\n"), expected(5));
        assert_eq!(request(&control, b"layout\n"), layout);
        assert!(input_request(&control, identity, "key 6 248 down\n").starts_with("error "));
        if enabled {
            assert_eq!(input_request(&control, identity, "key 7 125 down\n"), expected(6));
            assert!(input_request(&control, identity, "key 8 20 down\n").starts_with("unavailable "));
            let mut command = Command::new(env!("CARGO_BIN_EXE_td-compositor"));
            command.arg0("td-ctl").arg("--socket").arg(&control)
                .args(["release-keys", identity, "9"]);
            let output = root.0.join("ctl.out");
            let mut ctl = Process::to_file(&mut command, &root.0.join("ctl.log"), &output);
            assert!(ctl.wait().success());
            assert_eq!(fs::read_to_string(output).unwrap(),
                expected(7).strip_prefix("ok\n").unwrap());
        }
        // EOF terminates the owning keyboard generation, even with held keys.
        assert_eq!(input_request(&control, identity, "key 10 42 down\n"), expected(8));
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
    let ready = compositor.ready();
    let identity = ready_session(&ready);
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
    assert_eq!(input_request(&control, identity, "pointer 1 1 1 1 0 0\n"), input_receipt(identity, 1));
    assert_eq!(input_request(&control, identity, "release-input 2\n"), input_receipt(identity, 2));
    let layout = request(&control, b"layout\n");
    assert!(layout.contains("workspace active=1 "), "{layout}");
    assert!(layout.contains("visible=true focused=true"), "{layout}");
    for line in ["pointer 3 800 0 1 0 0\n", "pointer 3 0 600 1 0 0\n"] {
        assert_eq!(input_request(&control, identity, line), "error pointer coordinates outside the output\n");
    }
    assert_eq!(request(&control, b"layout\n"), layout);
    for (index, line) in [
        "pointer 4 300 300 1 0 0\n", "key 4 42 down\n",
        "release-keys 5\n", "pointer 6 310 310 1 1 -1\n", "release-input 7\n",
    ].into_iter().enumerate() {
        assert_eq!(input_request(&control, identity, line), input_receipt(identity, index as u64 + 3));
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
    ready_session(&compositor.ready());
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
