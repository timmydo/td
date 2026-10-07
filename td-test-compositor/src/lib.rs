//! The real headless `td-compositor` as td's native test harness: the one
//! copy of what each graphical client's `tests/support/native_compositor.rs`
//! drives it with. `Compositor` launches the binary `TD_TEST_COMPOSITOR`
//! names, reads its readiness line, and speaks its control socket: layout,
//! synthetic input with receipts, client observation, output capture and
//! clipboard control. `Directory` is a short-lived private directory at a
//! Linux-socket-length path for a session or a client's runtime.
//!
//! Test support only: a client names this crate under
//! `[dev-dependencies]`, and nothing it ships links it. A broken
//! expectation is a failed test, so the harness asserts and panics where a
//! production crate returns an error.

#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test support: a broken expectation fails the test that called it"
)]

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, String>;

/// The output every harness session runs at.
pub const OUTPUT_WIDTH: usize = 800;
pub const OUTPUT_HEIGHT: usize = 600;
/// One captured output as RGB rows.
pub const FRAME_BYTES: usize = OUTPUT_WIDTH * OUTPUT_HEIGHT * 3;

/// The bound on each harness wait. Every wait polls and returns once its
/// condition holds, so the bound costs a passing run nothing; it is wide
/// for a host loaded by parallel checks.
pub const TIMEOUT: Duration = Duration::from_secs(30);

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A short-lived private directory for a compositor session or a client's
/// runtime, at a Linux-socket-length path independent of TMPDIR, removed
/// with everything in it on drop.
pub struct Directory(pub PathBuf);

impl Directory {
    /// `/tmp/<prefix>-<pid>-<n>`, mode 0700 whatever the umask.
    pub fn new(prefix: &str) -> Self {
        // Concurrent hosted checks run in their own PID namespaces, so the
        // pid alone can repeat: only a directory this call created is ours.
        let mut taken = 0;
        loop {
            let path = Path::new("/tmp").join(format!(
                "{prefix}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && taken < 1024 => {
                    taken += 1;
                }
                created => {
                    created.unwrap();
                    // The create's mode is filtered by the umask.
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                        .unwrap();
                    return Self(path);
                }
            }
        }
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What is left before `deadline`, or `TimedOut` once nothing is.
pub fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "native I/O deadline"))
}

/// All of `bytes` to `stream` before `deadline`.
pub fn write_until(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
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

/// The value of the first whitespace-separated `key` token in `line`.
pub fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(key))
}

/// Whether `value` is a session identity: 32 lowercase hex digits.
pub fn identity(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A canonical decimal counter: digits, no leading zero.
pub fn number(value: &str) -> Result<u64> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("noncanonical compositor counter".into());
    }
    value
        .parse()
        .map_err(|_| "compositor counter overflow".into())
}

/// One `observe-client` answer: the client's identity counter, the commit
/// it last presented, the output frame that showed it, and whether that
/// commit is what the output shows now.
#[derive(Debug, Clone, Copy)]
pub struct Observation {
    pub client: u64,
    pub commit: u64,
    pub output: u64,
    pub current: bool,
}

/// Parses one `observe-client` reply for `window` in `session`.
pub fn observation(reply: &[u8], session: &str, window: &str) -> Result<Observation> {
    if reply.len() > 1024 {
        return Err("compositor observation limit".into());
    }
    let text = std::str::from_utf8(reply).map_err(|_| "compositor observation UTF-8")?;
    let prefix = format!("ok\ntd-client-v1 session={session} window={window} client=");
    let body = text
        .strip_prefix(&prefix)
        .ok_or("compositor observation identity")?;
    let (client, body) = body
        .split_once(" commit=")
        .ok_or("compositor client field")?;
    let (commit, body) = body
        .split_once(" output=")
        .ok_or("compositor commit field")?;
    let (output, current) = body
        .split_once(" current=")
        .ok_or("compositor output field")?;
    let observation = Observation {
        client: number(client)?,
        commit: number(commit)?,
        output: number(output)?,
        current: match current {
            "yes\n" => true,
            "no\n" => false,
            _ => return Err("compositor current field".into()),
        },
    };
    if observation.client == 0
        || (observation.current && (observation.commit == 0 || observation.output == 0))
    {
        return Err("invalid compositor observation counters".into());
    }
    Ok(observation)
}

/// Parses one `capture` reply in `session`: the output counter and the
/// frame's RGB rows.
pub fn ppm<'a>(reply: &'a [u8], session: &str) -> Result<(u64, &'a [u8])> {
    if reply.len() > FRAME_BYTES + 128 {
        return Err("compositor capture limit".into());
    }
    let prefix = format!("ok\nP6\n# td-output-v1 session={session} output=");
    let body = reply
        .strip_prefix(prefix.as_bytes())
        .ok_or("capture session or format")?;
    let newline = body
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or("capture output line")?;
    let output =
        number(std::str::from_utf8(&body[..newline]).map_err(|_| "capture counter UTF-8")?)?;
    let pixels = body[newline + 1..]
        .strip_prefix(b"800 600\n255\n")
        .ok_or("capture geometry")?;
    if output == 0 || pixels.len() != FRAME_BYTES {
        return Err("capture size or counter".into());
    }
    Ok((output, pixels))
}

/// A mapped toplevel and where the compositor composites it into the output,
/// read from the `layout` record rather than derived from the compositor's
/// tiling constants: an oracle then samples exactly the reported client rect.
#[derive(Debug, Clone)]
pub struct Placement {
    pub window: String,
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
    pub focused: bool,
}

/// The compositor's control surfaces a session enables beyond input.
#[derive(Clone, Copy, Debug, Default)]
pub struct Controls {
    pub capture: bool,
    pub clipboard: bool,
}

/// One headless compositor session, stopped by owner EOF.
pub struct Compositor {
    child: Child,
    directory: PathBuf,
    session: String,
    /// Input receipts counted so far; each injection expects the next.
    action: u64,
    output: Option<JoinHandle<()>>,
}

impl Compositor {
    /// Launches `TD_TEST_COMPOSITOR` headless at 800x600 with input control
    /// and `controls`, its session under `directory`, its stderr in
    /// `directory/compositor.log`, and waits for its readiness line.
    pub fn start(directory: &Path, controls: Controls) -> Self {
        let binary = PathBuf::from(
            std::env::var_os("TD_TEST_COMPOSITOR")
                .expect("set TD_TEST_COMPOSITOR to an explicitly built td-compositor"),
        );
        assert!(
            binary.is_absolute(),
            "compositor test tool must be an absolute path"
        );
        let session_dir = directory.join("session");
        let mut command = Command::new(binary);
        command
            .arg("headless")
            .arg("--session-dir")
            .arg(&session_dir)
            .args([
                "--width",
                "800",
                "--height",
                "600",
                "--input-control",
                "enabled",
            ]);
        if controls.capture {
            command.args(["--capture-control", "enabled"]);
        }
        if controls.clipboard {
            command.args(["--clipboard-control", "enabled"]);
        }
        let child = command
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(directory.join("compositor.log")).unwrap())
            .spawn()
            .unwrap();
        // Establish cleanup before any later setup or readiness can unwind.
        let mut compositor = Self {
            child,
            directory: session_dir,
            session: String::new(),
            action: 0,
            output: None,
        };
        let stdout = compositor.child.stdout.take().unwrap();
        let (send, receive) = mpsc::sync_channel(1);
        compositor.output = Some(
            std::thread::Builder::new()
                .spawn(move || {
                    let mut reader = BufReader::new(stdout);
                    let mut line = String::new();
                    if reader.by_ref().take(4097).read_line(&mut line).is_ok() && line.len() <= 4096
                    {
                        let _ = send.send(line);
                    }
                    let _ = io::copy(&mut reader, &mut io::sink());
                })
                .unwrap(),
        );
        let ready = receive
            .recv_timeout(TIMEOUT)
            .expect("compositor readiness deadline");
        let session = ready
            .strip_prefix("TD-COMPOSITOR-HEADLESS-READY version=2 session=")
            .and_then(|line| line.strip_suffix(" width=800 height=600 scale=1\n"))
            .expect("compositor readiness grammar");
        assert!(identity(session));
        compositor.session = session.to_string();
        compositor
    }

    /// The session's identity.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// The session directory, which holds the control and Wayland sockets
    /// and is gone once the compositor stops.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The session's Wayland socket, a client's `WAYLAND_DISPLAY`.
    pub fn display(&self) -> PathBuf {
        self.directory.join("wayland-0")
    }

    /// One control request and its whole reply, at most `limit` bytes.
    pub fn request(&self, line: &str, limit: usize) -> Vec<u8> {
        let deadline = Instant::now() + TIMEOUT;
        let mut stream = UnixStream::connect(self.directory.join("td-control")).unwrap();
        write_until(&mut stream, format!("{line}\n").as_bytes(), deadline).unwrap();
        let mut reply = Vec::new();
        let mut chunk = [0; 16384];
        loop {
            stream
                .set_read_timeout(Some(remaining(deadline).unwrap()))
                .unwrap();
            let count = match stream.read(&mut chunk) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result.expect("compositor reply"),
            };
            if count == 0 {
                break;
            }
            assert!(reply.len() + count <= limit, "compositor reply byte bound");
            reply.extend_from_slice(&chunk[..count]);
        }
        reply
    }

    /// The first mapped toplevel carrying `app_id`, and its placement.
    /// `None` until the client has bound, set its app id, and mapped a
    /// surface, so matching on the app id doubles as proof that
    /// `set_app_id` took effect.
    pub fn placement(&self, app_id: &str) -> Option<Placement> {
        self.placement_where(app_id, false)
    }

    /// The first mapped toplevel carrying `app_id` that has the keyboard,
    /// and its placement: `None` until the client has bound, set its app
    /// id and mapped a surface the seat's keys reach.
    pub fn focused_placement(&self, app_id: &str) -> Option<Placement> {
        self.placement_where(app_id, true)
    }

    fn placement_where(&self, app_id: &str, focused: bool) -> Option<Placement> {
        let layout = self.request("layout", 65536);
        let text = std::str::from_utf8(&layout).unwrap();
        text.lines()
            .filter_map(|line| line.strip_prefix("window "))
            .find(|line| {
                field(line, "app_id=") == Some(app_id)
                    && (!focused || field(line, "focused=") == Some("true"))
            })
            .map(|line| {
                let window = field(line, "id=").expect("layout window id").to_string();
                let id = window.strip_prefix('@').expect("layout window id sigil");
                assert!(number(id).expect("canonical layout window id") > 0);
                let axis = |key: &str| -> usize {
                    field(line, key)
                        .expect("layout rect field")
                        .parse()
                        .expect("canonical layout rect field")
                };
                Placement {
                    window,
                    x: axis("x="),
                    y: axis("y="),
                    width: axis("width="),
                    height: axis("height="),
                    focused: field(line, "focused=") == Some("true"),
                }
            })
    }

    /// Whether a toplevel carrying `app_id` is mapped and has the
    /// keyboard: the one activated toplevel, which the seat's keys reach.
    pub fn focused(&self, app_id: &str) -> bool {
        let layout = self.request("layout", 65536);
        let text = std::str::from_utf8(&layout).unwrap();
        text.lines()
            .filter_map(|line| line.strip_prefix("window "))
            .any(|line| {
                field(line, "app_id=") == Some(app_id) && field(line, "focused=") == Some("true")
            })
    }

    /// The client presenting `window`, observed now.
    pub fn observe(&self, window: &str) -> Observation {
        let request = format!("observe-client {} {window}", self.session);
        observation(&self.request(&request, 1024), &self.session, window).unwrap()
    }

    /// One synthetic input request through the compositor's input control
    /// and its receipt. Timestamps follow the receipt counter, so callers
    /// send no action IDs.
    pub fn receipt(&mut self, line: &str) {
        let action = self.action + 1;
        let expected = format!(
            "ok\ntd-action-v1 session={} action={action}\n",
            self.session
        );
        assert_eq!(self.request(line, 1024), expected.as_bytes());
        self.action = action;
    }

    /// The timestamp the next input request carries.
    pub fn next_time(&self) -> u64 {
        self.action + 1
    }

    /// One key event.
    pub fn key(&mut self, key: u32, down: bool) {
        let line = format!(
            "key {} {} {key} {}",
            self.session,
            self.next_time(),
            if down { "down" } else { "up" }
        );
        self.receipt(&line);
    }

    /// `key` pressed and released.
    pub fn tap(&mut self, key: u32) {
        self.key(key, true);
        self.key(key, false);
    }

    /// `key` tapped with every one of `modifiers` held, pressed in order
    /// and released in reverse.
    pub fn chord(&mut self, modifiers: &[u32], key: u32) {
        for modifier in modifiers {
            self.key(*modifier, true);
        }
        self.tap(key);
        for modifier in modifiers.iter().rev() {
            self.key(*modifier, false);
        }
    }

    /// One complete pointer report at output pixel `x`, `y` with the held
    /// button mask and the wheel's vertical and horizontal steps.
    pub fn pointer_frame(
        &mut self,
        x: usize,
        y: usize,
        buttons: u8,
        vertical: i32,
        horizontal: i32,
    ) {
        let line = format!(
            "pointer {} {} {x} {y} {buttons} {vertical} {horizontal}",
            self.session,
            self.next_time()
        );
        self.receipt(&line);
    }

    /// One pointer report with no wheel.
    pub fn pointer(&mut self, x: usize, y: usize, buttons: u8) {
        self.pointer_frame(x, y, buttons, 0, 0);
    }

    /// A press report and a release at `x`, `y`.
    pub fn click(&mut self, x: usize, y: usize) {
        self.pointer(x, y, 1);
        self.pointer(x, y, 0);
    }

    /// One clipboard control request, `clipboard-<verb>`, and its reply.
    pub fn clipboard(&self, verb: &str, operand: &str) -> String {
        let line = format!("clipboard-{verb} {} {operand}", self.session);
        String::from_utf8(self.request(&line, 1024)).unwrap()
    }

    /// The output now as RGB rows, with the output counter of the frame.
    pub fn capture(&self) -> (u64, Vec<u8>) {
        let capture = self.request("capture", FRAME_BYTES + 128);
        let (output, pixels) = ppm(&capture, &self.session).unwrap();
        (output, pixels.to_vec())
    }

    /// The window's tile as the output shows it now, under the observe,
    /// capture, observe rule: the frame is the same one before and after
    /// the capture, and the capture's output number lies after the first
    /// observation's and not after the second's, or the capture is retried.
    /// Returns the tile's RGB rows.
    pub fn tile(&self, place: &Placement) -> Vec<u8> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "no settled frame to capture");
            std::thread::sleep(Duration::from_millis(2));
            let first = self.observe(&place.window);
            if !first.current {
                continue;
            }
            let capture = self.request("capture", FRAME_BYTES + 128);
            let (output, pixels) = ppm(&capture, &self.session).unwrap();
            let second = self.observe(&place.window);
            if !second.current || second.commit != first.commit {
                continue;
            }
            assert_eq!(first.client, second.client);
            assert!(output > first.output && output <= second.output);
            let mut tile = Vec::with_capacity(place.width * place.height * 3);
            for y in 0..place.height {
                let start = ((place.y + y) * OUTPUT_WIDTH + place.x) * 3;
                tile.extend_from_slice(&pixels[start..start + place.width * 3]);
            }
            return tile;
        }
    }

    /// Owner EOF: the compositor exits successfully and removes its
    /// session directory.
    pub fn stop(&mut self) {
        self.child.stdin.take();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "compositor owner-EOF deadline");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!self.directory.exists());
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        // A failing case's directories go with it; what the compositor
        // said goes to the test's output first.
        if std::thread::panicking() {
            if let Some(parent) = self.directory.parent() {
                eprintln!(
                    "compositor log:\n{}",
                    std::fs::read_to_string(parent.join("compositor.log")).unwrap_or_default()
                );
            }
        }
        self.child.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(output) = self.output.take() {
            let _ = output.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_observation_and_capture_decoders_refuse_stale_or_truncated_evidence() {
        let session = "00000000000000000000000000000007";
        let reply = format!(
            "ok\ntd-client-v1 session={session} window=@1 client=2 commit=3 output=4 current=yes\n"
        );
        assert!(
            observation(reply.as_bytes(), session, "@1")
                .unwrap()
                .current
        );
        for end in 0..reply.len() {
            assert!(observation(&reply.as_bytes()[..end], session, "@1").is_err());
        }
        assert!(observation(reply.as_bytes(), "00000000000000000000000000000008", "@1").is_err());
        assert!(observation(reply.as_bytes(), session, "@2").is_err());
        for (from, to) in [
            ("client=2", "client=0"),
            ("commit=3", "commit=0"),
            ("commit=3", "commit=03"),
            ("output=4", "output=18446744073709551616"),
            ("current=yes\n", "current=yes\nextra\n"),
        ] {
            assert!(observation(reply.replace(from, to).as_bytes(), session, "@1").is_err());
        }
        let mut capture =
            format!("ok\nP6\n# td-output-v1 session={session} output=4\n800 600\n255\n")
                .into_bytes();
        let header = capture.len();
        capture.resize(header + FRAME_BYTES, 7);
        assert_eq!(ppm(&capture, session).unwrap().0, 4);
        for end in 0..header {
            assert!(ppm(&capture[..end], session).is_err());
        }
        assert!(ppm(&capture[..capture.len() - 1], session).is_err());
        assert!(ppm(&capture, "00000000000000000000000000000008").is_err());
        capture.push(7);
        assert!(ppm(&capture, session).is_err());
    }
}
