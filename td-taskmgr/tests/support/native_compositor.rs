use super::*;

use std::io::{BufRead, BufReader};
use std::sync::mpsc;
use std::thread::JoinHandle;

const OUTPUT_WIDTH: usize = 800;
const OUTPUT_HEIGHT: usize = 600;
const FRAME_BYTES: usize = OUTPUT_WIDTH * OUTPUT_HEIGHT * 3;

struct Compositor {
    child: Child,
    directory: PathBuf,
    session: String,
    output: Option<JoinHandle<()>>,
    /// Input receipts counted so far; each injection expects the next.
    action: u64,
}

/// A mapped toplevel and where the compositor composites it into the output,
/// read from the `layout` record rather than derived from the compositor's
/// tiling constants: the oracle then samples exactly the reported client rect.
#[derive(Debug, Clone)]
struct Placement {
    window: String,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(key))
}

#[derive(Debug, Clone, Copy)]
struct Observation {
    client: u64,
    commit: u64,
    output: u64,
    current: bool,
}

fn identity(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn number(value: &str) -> Result<u64> {
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

fn observation(reply: &[u8], session: &str, window: &str) -> Result<Observation> {
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

fn ppm<'a>(reply: &'a [u8], session: &str) -> Result<(u64, &'a [u8])> {
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

impl Compositor {
    fn start(directory: &Directory) -> Self {
        let binary = PathBuf::from(
            std::env::var_os("TD_TEST_COMPOSITOR")
                .expect("set TD_TEST_COMPOSITOR to an explicitly built td-compositor"),
        );
        assert!(
            binary.is_absolute(),
            "compositor test tool must be an absolute path"
        );
        let session_dir = directory.0.join("session");
        let child = Command::new(binary)
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
                "--capture-control",
                "enabled",
            ])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(directory.0.join("compositor.log")).unwrap())
            .spawn()
            .unwrap();
        // Establish cleanup before any later setup or readiness can unwind.
        let mut compositor = Self {
            child,
            directory: session_dir,
            session: String::new(),
            output: None,
            action: 0,
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
                    let _ = std::io::copy(&mut reader, &mut std::io::sink());
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

    fn request(&self, line: &str, limit: usize) -> Vec<u8> {
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

    /// The one mapped toplevel carrying `app_id`, and its placement. `None`
    /// until the client has bound, set its app id, and mapped a surface, so
    /// matching on the app id doubles as proof that `set_app_id` took effect.
    fn placement(&self, app_id: &str) -> Option<Placement> {
        let layout = self.request("layout", 65536);
        let text = std::str::from_utf8(&layout).unwrap();
        text.lines()
            .filter_map(|line| line.strip_prefix("window "))
            .find(|line| field(line, "app_id=") == Some(app_id))
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
                }
            })
    }

    fn observe(&self, window: &str) -> Observation {
        let request = format!("observe-client {} {window}", self.session);
        observation(&self.request(&request, 1024), &self.session, window).unwrap()
    }

    /// One synthetic input request through the compositor's input control
    /// and its receipt. Timestamps follow the receipt counter.
    fn receipt(&mut self, line: &str) {
        let action = self.action + 1;
        let expected = format!(
            "ok\ntd-action-v1 session={} action={action}\n",
            self.session
        );
        assert_eq!(self.request(line, 1024), expected.as_bytes());
        self.action = action;
    }

    /// One key event.
    fn key(&mut self, key: u32, down: bool) {
        let line = format!(
            "key {} {} {key} {}",
            self.session,
            self.action + 1,
            if down { "down" } else { "up" }
        );
        self.receipt(&line);
    }

    /// One complete pointer report at output pixel `x`, `y` with the held
    /// button mask and no wheel; a click is a press report and a release.
    fn pointer(&mut self, x: usize, y: usize, buttons: u8) {
        let line = format!(
            "pointer {} {} {x} {y} {buttons} 0 0",
            self.session,
            self.action + 1
        );
        self.receipt(&line);
    }

    fn click(&mut self, x: usize, y: usize) {
        self.pointer(x, y, 1);
        self.pointer(x, y, 0);
    }

    /// The window's tile as the output shows it now, under the observe,
    /// capture, observe rule: the frame is the same one before and after
    /// the capture, and the capture's output number lies after the first
    /// observation's and not after the second's, or the capture is retried.
    /// Returns the tile's RGB rows.
    fn tile(&self, place: &Placement) -> Vec<u8> {
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

    fn stop(&mut self) {
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

/// One owned client with a private optional control endpoint.
pub(super) struct TaskProcess {
    child: Child,
    log: PathBuf,
    socket: PathBuf,
}
impl TaskProcess {
    pub(super) fn start(directory: &Directory, display: &Path) -> Self {
        let log = directory.0.join("stderr");
        let socket = directory.0.join("control");
        let child = Command::new(env!("CARGO_BIN_EXE_td-taskmgr"))
            .arg("--control-socket")
            .arg(&socket)
            .env_clear()
            .env("WAYLAND_DISPLAY", display)
            .env("XDG_RUNTIME_DIR", &directory.0)
            .env("TMPDIR", &directory.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        Self { child, log, socket }
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// One request over the window's control socket: the seam's envelope
    /// out, the reply's fields after the version and the ID back.
    pub(super) fn request(&self, id: u64, words: &[&str]) -> Vec<String> {
        let deadline = Instant::now() + TIMEOUT;
        let mut stream = UnixStream::connect(&self.socket).unwrap();
        let payload = format!("1\t{id}\t{}", words.join("\t"));
        write_until(&mut stream, &frame(payload.as_bytes()).unwrap(), deadline).unwrap();
        let mut decoder = Decoder::default();
        let mut chunk = [0u8; 4096];
        loop {
            stream
                .set_read_timeout(Some(remaining(deadline).unwrap()))
                .unwrap();
            let count = match stream.read(&mut chunk) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result.expect("control reply"),
            };
            if count == 0 {
                break;
            }
            decoder.push(&chunk[..count]).unwrap();
            if decoder.payload().is_some() {
                break;
            }
        }
        assert!(
            decoder.payload().is_some(),
            "request {id} {words:?}: no complete reply; client stderr: {}",
            self.stderr()
        );
        let line = String::from_utf8(decoder.finish().unwrap()).unwrap();
        let fields: Vec<String> = line.split('\t').map(str::to_string).collect();
        assert_eq!(fields[0], "1", "{line}");
        assert_eq!(fields[1], id.to_string(), "{line}");
        fields[2..].to_vec()
    }

    /// Waits for the window to exit and says whether it exited well.
    pub(super) fn finish(mut self) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.success();
            }
            assert!(
                Instant::now() < deadline,
                "td-taskmgr did not exit; stderr: {}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for TaskProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn state(client: &TaskProcess) -> std::collections::BTreeMap<String, String> {
    let fields = client.request(1, &["state"]);
    assert_eq!(fields.first().map(String::as_str), Some("ok"));
    fields
        .into_iter()
        .skip(1)
        .map(|field| {
            let (key, value) = field.split_once('=').unwrap();
            (key.to_owned(), value.to_owned())
        })
        .collect()
}
pub(super) fn counter(state: &std::collections::BTreeMap<String, String>, key: &str) -> u64 {
    state.get(key).unwrap().parse().unwrap()
}
pub(super) fn wait_state(
    client: &TaskProcess,
    condition: impl Fn(&std::collections::BTreeMap<String, String>) -> bool,
) -> std::collections::BTreeMap<String, String> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let state = state(client);
        if condition(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "state deadline: {state:?}; {}",
            client.stderr()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
pub(super) fn chord(client: &TaskProcess, key: &str) {
    let reply = client.request(2, &["key", &td_ui::control::hex(key.as_bytes())]);
    assert_eq!(reply.first().map(String::as_str), Some("ok"), "{reply:?}");
}
fn tap(compositor: &mut Compositor, key: u32) {
    compositor.key(key, true);
    compositor.key(key, false);
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn live_history_graph_tree_and_hidden_window_collection() {
    let server_directory = Directory::new();
    let mut compositor = Compositor::start(&server_directory);
    let client_directory = Directory::new();
    let client = TaskProcess::start(&client_directory, &compositor.directory.join("wayland-0"));
    let deadline = Instant::now() + TIMEOUT;
    let place = loop {
        if let Some(place) = compositor.placement("td-taskmgr") {
            break place;
        }
        assert!(
            Instant::now() < deadline,
            "mapping deadline: {}",
            client.stderr()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(place.x + place.width <= OUTPUT_WIDTH && place.y + place.height <= OUTPUT_HEIGHT);
    for _ in 0..3 {
        chord(&client, "C-i");
    }
    wait_state(&client, |s| {
        counter(s, "retained") >= 3 && counter(s, "rows") > 0
    });
    let before = compositor.tile(&place);
    assert!(before.as_chunks::<3>().0.iter().any(|p| p != &before[..3]));
    // Physical click on the CPU tab followed by Tab and Home through the seat.
    compositor.click(place.x + 200, place.y + 10);
    wait_state(&client, |s| s.get("tab").is_some_and(|s| s == "CPU"));
    tap(&mut compositor, 15);
    wait_state(&client, |s| s.get("focus").is_some_and(|s| s == "Graph"));
    tap(&mut compositor, 102);
    tap(&mut compositor, 57); // Clear the series: ranked contributors at this time.
    let inspected = wait_state(&client, |s| s.get("live").is_some_and(|s| s == "false"));
    let pinned = counter(&inspected, "inspected_ns");
    tap(&mut compositor, 28);
    let selected = wait_state(&client, |s| {
        s.get("selected")
            .is_some_and(|s| s != "none" && s != "group")
    });
    let key = selected.get("selected").unwrap().clone();
    chord(&client, "C-f");
    for scalar in "no-such-taskmgr-fixture".chars() {
        chord(&client, &scalar.to_string());
    }
    let filtered = state(&client);
    assert_eq!(filtered.get("selected"), Some(&key));
    assert!(
        counter(&filtered, "rows") > 0,
        "selected search exception remains visible"
    );
    compositor.click(place.x + 350, place.y + 10);
    let memory = wait_state(&client, |s| s.get("tab").is_some_and(|s| s == "Memory"));
    assert_eq!(counter(&memory, "inspected_ns"), pinned);
    assert_eq!(memory.get("selected"), Some(&key));
    let after = compositor.tile(&place);
    assert_ne!(before, after);
    // A hidden window must keep consuming observations without a frame backlog.
    compositor.key(125, true);
    tap(&mut compositor, 3);
    compositor.key(125, false);
    let start = state(&client);
    let newer = wait_state(&client, |s| {
        counter(s, "newest_ns") > counter(&start, "newest_ns") + 1_500_000_000
    });
    assert_eq!(counter(&newer, "inspected_ns"), pinned);
    let hidden_frames = counter(&newer, "presentations");
    let latest = wait_state(&client, |s| {
        counter(s, "newest_ns") > counter(&newer, "newest_ns") + 1_000_000_000
    });
    // td-compositor currently completes hidden-client frame callbacks too.
    // The app must still coalesce each update and retain only bounded data.
    assert!(counter(&latest, "presentations") <= hidden_frames + 10);
    assert!(counter(&latest, "model_bytes") <= td_taskmgr::budget::LIMIT as u64);
    compositor.key(125, true);
    tap(&mut compositor, 2);
    compositor.key(125, false);
    chord(&client, "C-l");
    wait_state(&client, |s| {
        s.get("live").is_some_and(|s| s == "true") && counter(s, "presentations") > hidden_frames
    });
    let _ = compositor.tile(&place);
    assert_eq!(client.request(3, &["action", "quit"]), ["ok", "quit"]);
    assert!(client.finish());
    compositor.stop();
}
