//! The real headless `td-compositor` as a render and transport oracle for
//! the td-photo window. A generic `Compositor` (the same control protocol
//! the td-editor and td-setup harnesses drive) launches the compositor from
//! the `TD_TEST_COMPOSITOR` binary, and `PhotoProcess` launches the window
//! as an ordinary client against its Wayland socket, serving the seam on a
//! control socket of its own. The one case proves the live path the replay
//! cannot: the window binds, obeys the configure, presents the roll, answers
//! the socket in the replay's vocabulary, holds `wait-idle` until the frame
//! on screen is the model's, writes a flag through the sidecar and shows it,
//! takes a click and a key from the seat, and closes on `quit`; its captured
//! pixels equal the crate's own `--preview` of the same roll at the same
//! size, before the flag and after.

use super::*;

use std::io::{BufRead, BufReader};
use std::sync::mpsc;
use std::thread::JoinHandle;

const OUTPUT_WIDTH: usize = 800;
const OUTPUT_HEIGHT: usize = 600;
const FRAME_BYTES: usize = OUTPUT_WIDTH * OUTPUT_HEIGHT * 3;
/// evdev's KEY_END, the chord `End`, which `last` binds: the last photo from
/// anywhere, so a press repeated until one lands moves the cursor once.
const KEY_END: u32 = 107;

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

/// The td-photo window launched as an ordinary Wayland client against the
/// compositor's socket, on a roll, serving the seam on a control socket in
/// the client's private directory. Its cache is private too, so a
/// developer's cache is neither read nor filled.
struct PhotoProcess {
    child: Child,
    log: PathBuf,
    socket: PathBuf,
}
impl PhotoProcess {
    fn start(directory: &Directory, display: &Path, roll: &Path) -> Self {
        let log = directory.0.join("stderr");
        let socket = directory.0.join("control");
        let child = Command::new(env!("CARGO_BIN_EXE_td-photo"))
            .arg("open")
            .arg(roll)
            .arg("--control-socket")
            .arg(&socket)
            .env_clear()
            .env("WAYLAND_DISPLAY", display)
            .env("XDG_RUNTIME_DIR", &directory.0)
            .env("TMPDIR", &directory.0)
            .env("XDG_CACHE_HOME", directory.0.join("cache"))
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
    fn request(&self, id: u64, words: &[&str]) -> Vec<String> {
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

    /// `wait-idle` until the window says so, within the harness deadline.
    fn settle(&self, id: u64) {
        let reply = self.request(id, &["wait-idle", "4000"]);
        assert_eq!(reply, ["ok", "idle"], "stderr: {}", self.stderr());
    }

    /// Waits for the window to exit and says whether it exited well.
    fn finish(mut self) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.success();
            }
            assert!(
                Instant::now() < deadline,
                "td-photo did not exit; stderr: {}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for PhotoProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `td-photo --preview WxH ROLL`, the RGB rows of its PPM, under the same
/// private cache as the window.
fn preview(directory: &Directory, width: usize, height: usize, roll: &Path) -> Vec<u8> {
    let output = Command::new(env!("CARGO_BIN_EXE_td-photo"))
        .arg("--preview")
        .arg(format!("{width}x{height}"))
        .arg(roll)
        .env_clear()
        .env("XDG_CACHE_HOME", directory.0.join("cache"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let header = format!("P6\n{width} {height}\n255\n");
    let pixels = output
        .stdout
        .strip_prefix(header.as_bytes())
        .expect("preview PPM header");
    assert_eq!(pixels.len(), width * height * 3);
    pixels.to_vec()
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn the_window_presents_the_roll_and_answers_the_socket_over_the_native_compositor() {
    let compositor_directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let client_directory = Directory::new();
    // Three originals no decoder accepts, so every thumbnail box keeps its
    // placeholder and the frame is the scene's alone: the same in the
    // window and in `--preview`. The second is a reject already.
    let roll = client_directory.0.join("roll");
    std::fs::create_dir(&roll).unwrap();
    for name in ["DSC_0001.NEF", "DSC_0002.NEF", "DSC_0003.NEF"] {
        std::fs::write(roll.join(name), b"not really a nef").unwrap();
    }
    std::fs::write(
        roll.join("DSC_0002.NEF.edit"),
        "td-photo edit 1\nflag reject\n",
    )
    .unwrap();
    let client = PhotoProcess::start(
        &client_directory,
        &compositor.directory.join("wayland-0"),
        &roll,
    );

    // Wait for the client to bind, set its app id, and map its one toplevel;
    // the compositor then reports the tile it composited it into.
    let deadline = Instant::now() + TIMEOUT;
    let place = loop {
        if let Some(place) = compositor.placement("td-photo") {
            break place;
        }
        assert!(
            Instant::now() < deadline,
            "td-photo window never mapped; client stderr: {}",
            client.stderr()
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(
        place.x + place.width <= OUTPUT_WIDTH && place.y + place.height <= OUTPUT_HEIGHT,
        "reported tile {place:?} exceeds the {OUTPUT_WIDTH}x{OUTPUT_HEIGHT} output"
    );

    // Idle means the thumbnails were attempted and the frame on screen is
    // the model's at the tile's extent; the state says the roll is open with
    // no job outstanding, and the captured tile equals the crate's own
    // preview of that roll at that size.
    client.settle(1);
    let state = client.request(2, &["state"]);
    assert_eq!(&state[..2], ["ok", "cull"], "{state:?}");
    assert_eq!(&state[3..8], ["3", "3", "0", "all", "grid"], "{state:?}");
    assert_eq!(state[14], "0", "jobs outstanding: {state:?}");
    let generation = state[15].clone();
    let before = preview(&client_directory, place.width, place.height, &roll);
    assert_eq!(compositor.tile(&place), before, "the first frame");

    // A flag over the socket is written through the sidecar and shown: the
    // frame changes and equals the preview of the roll as it now is.
    assert_eq!(client.request(3, &["action", "pick"]), ["ok", "changed"]);
    assert_eq!(
        std::fs::read_to_string(roll.join("DSC_0001.NEF.edit")).unwrap(),
        "td-photo edit 1\nflag pick\n"
    );
    client.settle(4);
    let after = preview(&client_directory, place.width, place.height, &roll);
    assert_ne!(after, before);
    assert_eq!(compositor.tile(&place), after, "the frame after the pick");
    let state = client.request(5, &["state"]);
    assert_eq!(state[9], "pick", "{state:?}");
    assert_ne!(state[15], generation);

    // A press from the seat reaches the same dispatcher: a click on the
    // second cell selects it. The map, the seat's devices and the pointer's
    // focus arrive on the seat's own schedule, so a click before the client
    // can take one is repeated until one lands; selecting the second cell
    // again is the same cell, so the cursor is 1 whatever lands after. The
    // cell's box is the model's own layout for the tile, in output pixels.
    let layout = td_photo::ui::Layout::new(
        Surface::new(place.width, place.height, Scale::default()).unwrap(),
    );
    let second = layout.thumb(layout.cell(1, 0).expect("a second cell on the tile"));
    let (x, y) = (
        place.x + second.x as usize + second.width as usize / 2,
        place.y + second.y as usize + second.height as usize / 2,
    );
    let deadline = Instant::now() + TIMEOUT;
    let mut id = 6;
    let cursor = loop {
        compositor.click(x, y);
        client.settle(id);
        let state = client.request(id + 1, &["state"]);
        id += 2;
        if state[5] != "0" {
            break state[5].clone();
        }
        assert!(
            Instant::now() < deadline,
            "no click reached the window; client stderr: {}",
            client.stderr()
        );
    };
    assert_eq!(cursor, "1");
    // The seat draws the client's cursor where the pointer is, which no
    // preview has, so it is parked in the desktop bar before the captures
    // to come: the compositor's own cross there reaches six pixels.
    assert!(place.y > 6, "no desktop bar above the tile: {place:?}");
    compositor.pointer(0, 0, 0);
    // A key likewise: `End` is the last photo from anywhere, so a press
    // repeated until one lands leaves the cursor at 2 whatever lands after.
    let deadline = Instant::now() + TIMEOUT;
    let cursor = loop {
        compositor.key(KEY_END, true);
        compositor.key(KEY_END, false);
        client.settle(id);
        let state = client.request(id + 1, &["state"]);
        id += 2;
        if state[5] != "1" {
            break state[5].clone();
        }
        assert!(
            Instant::now() < deadline,
            "no key reached the window; client stderr: {}",
            client.stderr()
        );
    };
    assert_eq!(cursor, "2");
    // The cursor's frame differs from the pick's and is the preview's again:
    // the preview opens the roll with the cursor at the first photo, so it
    // is the frame after `first`, which the socket applies to prove the
    // seat and the request share the model.
    let moved = compositor.tile(&place);
    assert_ne!(moved, after);
    assert_eq!(client.request(id, &["action", "first"]), ["ok", "changed"]);
    client.settle(id + 1);
    id += 2;
    assert_eq!(
        compositor.tile(&place),
        after,
        "the frame back at the first photo"
    );

    // `quit` closes the window, which exits well and takes its socket away.
    assert_eq!(client.request(id, &["action", "quit"]), ["ok", "quit"]);
    let socket = client.socket.clone();
    assert!(client.finish(), "td-photo exited with a failure");
    assert!(!socket.exists(), "the control socket was left behind");
    compositor.stop();
}

/// The status row, the last line of `text`, is `full`, or, when the tile is
/// narrower than the row, its head ended with an ellipsis; either way the row shown
/// reaches `note`, the start of the note under test. The wide replay test
/// in `tests/ui.rs` holds the whole row.
fn shows(text: &str, full: &str, note: &str) {
    let row = text.lines().last().unwrap_or("").trim_start();
    let head = row.strip_suffix('\u{2026}').unwrap_or(row);
    assert!(
        full.starts_with(head) && head.contains(note),
        "status row {row:?} does not show {full:?} up to {note:?}"
    );
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn the_window_develops_the_cursor_photo_over_the_native_compositor() {
    let compositor_directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let client_directory = Directory::new();
    // One decodable synthetic NEF, a gradient so the developed frame is not
    // uniform, so the develop box carries an image the placeholder is not;
    // its embedded preview a flat JPEG, so the filmstrip's box carries a
    // thumbnail the live window blits as `--preview` does.
    let roll = client_directory.0.join("roll");
    std::fs::create_dir(&roll).unwrap();
    let (w, h) = (64usize, 48usize);
    let samples: Vec<u16> = (0..w * h).map(|i| 1008 + (i as u16 % 4000)).collect();
    let thumb = [0x30u8, 0x70, 0xb0];
    std::fs::write(
        roll.join("DSC_0001.NEF"),
        super::synth_nef::nef_with_preview(w, h, &samples, &super::flat_jpeg(w, h, thumb)),
    )
    .unwrap();
    let client = PhotoProcess::start(
        &client_directory,
        &compositor.directory.join("wayland-0"),
        &roll,
    );

    // Wait for the client to bind, set its app id, and map its one toplevel.
    let deadline = Instant::now() + TIMEOUT;
    let place = loop {
        if let Some(place) = compositor.placement("td-photo") {
            break place;
        }
        assert!(
            Instant::now() < deadline,
            "td-photo window never mapped; client stderr: {}",
            client.stderr()
        );
        std::thread::sleep(Duration::from_millis(2));
    };

    // Develop over the socket: the cursor photo is developed into the box.
    // Idle means the developed frame is on screen, and the captured tile
    // equals the crate's own developed preview of that photo at that size.
    // The pointer is never injected, so the client's cursor is not over the
    // tile as it was not in the cull case's first capture.
    assert_eq!(client.request(1, &["action", "develop"]), ["ok", "changed"]);
    client.settle(2);
    let state = client.request(3, &["state"]);
    assert_eq!(&state[..2], ["ok", "develop"], "{state:?}");
    // The window's layout has the built-in looks in its look band (the
    // client runs with a home that has no user looks), which place the box.
    let layout = super::binary_layout(place.width, place.height);
    let r#box = layout.develop_box().expect("a develop box on the tile");
    let developed = super::preview_develop(&client_directory, place.width, place.height, &roll, 0);
    assert!(
        super::varies(&developed, place.width, r#box),
        "the develop box carries no developed image"
    );
    let tile = compositor.tile(&place);
    assert_eq!(tile, developed, "the developed frame");
    // The strip's one box carries the thumbnail in the live frame: the
    // window's own blit loop, not only `--preview`'s.
    let film = layout.film_boxes(1, 0);
    let (_, strip) = film.first().expect("a filmstrip box on the tile");
    let middle = (strip.y as usize + strip.height as usize / 2) * place.width
        + strip.x as usize
        + strip.width as usize / 2;
    let at = &tile[middle * 3..middle * 3 + 3];
    assert!(
        at.iter().zip(thumb).all(|(p, q)| p.abs_diff(q) <= 4),
        "the filmstrip box shows {at:?}, not the thumbnail {thumb:?}"
    );

    // An exposure edit over the socket re-develops: the frame changes and is
    // the developed preview of the roll as its sidecar now is.
    assert_eq!(
        client.request(4, &["action", "expose-in"]),
        ["ok", "changed"]
    );
    client.settle(5);
    assert_eq!(
        std::fs::read_to_string(roll.join("DSC_0001.NEF.edit")).unwrap(),
        "td-photo edit 1\nexposure 0.33\nstep-1 on exposure 0.33\n"
    );
    let brighter = super::preview_develop(&client_directory, place.width, place.height, &roll, 0);
    // The develop box itself changes, not merely the facts line's exposure
    // text, so the exposure reached the developed pixels.
    assert_ne!(
        super::box_pixels(&developed, place.width, r#box),
        super::box_pixels(&brighter, place.width, r#box),
        "the exposure did not reach the developed pixels"
    );
    assert_eq!(
        compositor.tile(&place),
        brighter,
        "the developed frame after the exposure"
    );

    // A look the sidecar names but no file provides makes the develop for the
    // same photo fail: the box is redrawn to the neutral placeholder, not left
    // showing the last exposure's pixels, and `wait-idle` still settles.
    // Without a redraw on a failed develop the box would keep the stale image
    // and idle would be reported over it; the frame equals `--preview
    // --develop` of the roll as its sidecar now is.
    assert_eq!(
        client.request(6, &["action", "look", "no-such-look"]),
        ["ok", "changed"]
    );
    client.settle(7);
    let placeholder =
        super::preview_develop(&client_directory, place.width, place.height, &roll, 0);
    assert!(
        !super::varies(&placeholder, place.width, r#box),
        "the develop box is not a placeholder after a failed develop"
    );
    assert_ne!(
        super::box_pixels(&brighter, place.width, r#box),
        super::box_pixels(&placeholder, place.width, r#box),
        "the box still shows the last developed pixels after a failed develop"
    );
    assert_eq!(
        compositor.tile(&place),
        placeholder,
        "the placeholder frame after a failed develop"
    );

    // An export over the socket runs on the pool: `wait-idle` waits for it,
    // the JPEG is in the roll's `exported/` when idle, and the status row
    // says so. The look the sidecar still names is not there, so the
    // export fails as the develop did, and the row says that too.
    assert_eq!(client.request(20, &["action", "export"]), ["ok", "changed"]);
    client.settle(21);
    let text = client.request(22, &["text"]);
    let row = String::from_utf8(td_ui::control::unhex(&text[3]).unwrap()).unwrap();
    shows(
        &row,
        "roll | 1 photos, 1 shown | all | 1/1 DSC_0001.NEF unflagged | develop | export of DSC_0001.NEF failed",
        "| export of DSC_0001.",
    );
    assert!(!roll.join("exported").exists());
    assert_eq!(
        client.request(23, &["action", "look", "-"]),
        ["ok", "changed"]
    );
    assert_eq!(client.request(24, &["action", "export"]), ["ok", "changed"]);
    client.settle(25);
    let text = client.request(26, &["text"]);
    let row = String::from_utf8(td_ui::control::unhex(&text[3]).unwrap()).unwrap();
    shows(
        &row,
        "roll | 1 photos, 1 shown | all | 1/1 DSC_0001.NEF unflagged | develop | exported DSC_0001.jpg",
        "| exported DSC_0001.",
    );
    let jpeg = std::fs::read(roll.join("exported/DSC_0001.jpg")).unwrap();
    let head = td_photo::jpeg::header(&jpeg).unwrap();
    assert_eq!((head.width, head.height), (60, 44));

    // An export asked for and not waited for: `quit` closes the window,
    // which exits well and takes its socket away, once the pool has
    // written the export (the second of the name, numbered).
    assert_eq!(client.request(27, &["action", "export"]), ["ok", "changed"]);
    assert_eq!(client.request(28, &["action", "quit"]), ["ok", "quit"]);
    let socket = client.socket.clone();
    assert!(client.finish(), "td-photo exited with a failure");
    assert!(!socket.exists(), "the control socket was left behind");
    let jpeg = std::fs::read(roll.join("exported/DSC_0001-2.jpg")).unwrap();
    let head = td_photo::jpeg::header(&jpeg).unwrap();
    assert_eq!((head.width, head.height), (60, 44));
    compositor.stop();
}
