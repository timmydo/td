//! The real headless `td-compositor` as a render oracle for the file chooser.
//! A generic `Compositor` (the same control protocol the td-editor and td-setup
//! native harnesses drive) launches the compositor from `TD_TEST_COMPOSITOR`,
//! and a minimal in-process `ChooserClient` maps one toplevel that presents
//! `Chooser::render_sized`. The portal binary is a D-Bus service, not a
//! spawnable Wayland client, and its manager global lives only on the private
//! socket, so this case drives a bare client over the public compositor: it
//! proves the chooser's real pixels survive the live compositor round-trip,
//! while `tests/dialog.rs` covers the manager protocol the wire cannot.

use super::*;

use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use td_portal::file_chooser::{Chooser, Mode};
use td_ui::client::{run, App, Client, Handled, Tag};
use td_ui::wire::Message;

const OUTPUT_WIDTH: usize = 800;
const OUTPUT_HEIGHT: usize = 600;
const FRAME_BYTES: usize = OUTPUT_WIDTH * OUTPUT_HEIGHT * 3;
const APP_ID: &str = "td-portal-file-chooser";

struct Compositor {
    child: Child,
    directory: PathBuf,
    session: String,
    output: Option<JoinHandle<()>>,
}

/// A mapped toplevel and where the compositor composites it, read from the
/// `layout` record rather than derived from tiling constants.
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
    let (client, body) = body.split_once(" commit=").ok_or("compositor client field")?;
    let (commit, body) = body.split_once(" output=").ok_or("compositor commit field")?;
    let (output, current) = body.split_once(" current=").ok_or("compositor output field")?;
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
        let mut compositor = Self {
            child,
            directory: session_dir,
            session: String::new(),
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

/// The chooser client owns no Wayland objects of its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Object {}

impl Tag for Object {
    fn retired(self) -> bool {
        match self {}
    }
}

/// A minimal live client: it maps one toplevel and presents the chooser at the
/// compositor's configured extent, then stops when asked. It reads nothing
/// from the seat, so seat, keyboard, pointer and clipboard events are no-ops.
struct ChooserClient {
    client: Client<Object>,
    chooser: Chooser,
    size: (usize, usize),
    dirty: bool,
    stop: Arc<AtomicBool>,
}

impl ChooserClient {
    fn new(stream: UnixStream, temporary: PathBuf, chooser: Chooser, stop: Arc<AtomicBool>) -> Self {
        Self {
            client: Client::new(stream, temporary).unwrap(),
            chooser,
            size: (td_portal::file_chooser::WIDTH, td_portal::file_chooser::HEIGHT),
            dirty: true,
            stop,
        }
    }
}

impl App for ChooserClient {
    type Tag = Object;

    fn client(&mut self) -> &mut Client<Object> {
        &mut self.client
    }

    fn needs_descriptor(&self, _: &Message) -> Result<bool> {
        Ok(false)
    }

    fn descriptor_wait(&mut self) {}

    fn tick(&mut self, _now: u64) -> Result<()> {
        Ok(())
    }

    fn event(&mut self, message: Message) -> Result<()> {
        match self.client.handle(&message, 0)? {
            Handled::Done | Handled::FrameDone => Ok(()),
            Handled::Bound => {
                self.client.set_title("Open file")?;
                self.client.set_app_id(APP_ID)?;
                self.client.commit()
            }
            Handled::Configure { size, serial } => {
                if let Some((width, height)) = size {
                    let (current_w, current_h) = self.size;
                    self.size = (
                        if width <= 0 { current_w } else { width as usize },
                        if height <= 0 { current_h } else { height as usize },
                    );
                    self.dirty = true;
                }
                self.client.acknowledge(serial)
            }
            Handled::CloseRequested => {
                self.client.close();
                Ok(())
            }
            Handled::GlobalRemoved { required: true, .. } => {
                Err("required Wayland global was removed".into())
            }
            Handled::GlobalRemoved { .. }
            | Handled::Capabilities { .. }
            | Handled::Keyboard(_)
            | Handled::Pointer(_)
            | Handled::Clipboard(_)
            | Handled::SeatRemoved => Ok(()),
            Handled::Unhandled => Err(format!(
                "unexpected Wayland event {}:{}",
                message.object, message.opcode
            )),
        }
    }

    fn end_turn(&mut self, _now: u64, _idle: bool) -> Result<()> {
        if self.stop.load(Ordering::Relaxed) {
            self.client.close();
        }
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        if !self.dirty || !self.client.can_present() {
            return Ok(());
        }
        let (width, height) = self.size;
        let Self { client, chooser, .. } = self;
        let presented = client.present(width, height, &mut |pixels| {
            let frame = chooser.render_sized(width, height)?;
            if frame.len() != pixels.len() {
                return Err("chooser frame size".into());
            }
            pixels.copy_from_slice(&frame);
            Ok(())
        })?;
        if presented {
            self.dirty = false;
        }
        Ok(())
    }
}

/// A directory with one file, so the chooser has a deterministic entry to
/// render both in the client and in the oracle.
fn chooser_root(directory: &Directory) -> PathBuf {
    let root = directory.0.join("Downloads");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("alpha.txt"), b"native fixture").unwrap();
    root
}

fn open_chooser(root: &Path) -> Chooser {
    Chooser::open_with_options(
        "Open file",
        root,
        Path::new("/home/td/Downloads"),
        Mode::OpenFile { multiple: false },
        None,
        None,
    )
    .unwrap()
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn chooser_presents_over_the_native_compositor() {
    let compositor_directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);

    let client_directory = Directory::new();
    let root = chooser_root(&client_directory);
    let socket = compositor.directory.join("wayland-0");
    let stop = Arc::new(AtomicBool::new(false));
    let client_stop = stop.clone();
    let client_runtime = client_directory.0.clone();
    let client_root = root.clone();
    let client = std::thread::spawn(move || -> Result<()> {
        let stream = UnixStream::connect(&socket).map_err(|e| format!("connect compositor: {e}"))?;
        let chooser = open_chooser(&client_root);
        let mut app = ChooserClient::new(stream, client_runtime, chooser, client_stop);
        run(&mut app)
    });

    // Wait for the client to bind, set its app id, and map its one toplevel;
    // the compositor then reports the tile it composited it into.
    let deadline = Instant::now() + TIMEOUT;
    let place = loop {
        if let Some(place) = compositor.placement(APP_ID) {
            break place;
        }
        assert!(
            Instant::now() < deadline,
            "chooser window never mapped; client finished: {}",
            client.is_finished()
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(
        place.x + place.width <= OUTPUT_WIDTH && place.y + place.height <= OUTPUT_HEIGHT,
        "reported tile {place:?} exceeds the {OUTPUT_WIDTH}x{OUTPUT_HEIGHT} output"
    );

    // The client obeys the compositor's configure and presents the chooser at
    // the tile's extent, so the captured pixels equal the crate's own render of
    // that same surface. A capture may still be the earlier default-extent
    // frame clipped to the tile, so the loop waits for the settled frame under
    // a two-observation sandwich around the capture, within its own deadline.
    let expected = open_chooser(&root)
        .render_sized(place.width, place.height)
        .unwrap();
    let render_deadline = Instant::now() + TIMEOUT;
    let mut rendered = false;
    while Instant::now() < render_deadline {
        std::thread::sleep(Duration::from_millis(2));
        let first = compositor.observe(&place.window);
        if !first.current {
            continue;
        }
        let capture = compositor.request("capture", FRAME_BYTES + 128);
        let (output, pixels) = ppm(&capture, &compositor.session).unwrap();
        let second = compositor.observe(&place.window);
        if !second.current || second.commit != first.commit {
            continue;
        }
        assert_eq!(first.client, second.client);
        assert!(output >= first.output && output <= second.output);
        let matches = (0..place.height).all(|y| {
            (0..place.width).all(|x| {
                let source = ((place.y + y) * OUTPUT_WIDTH + place.x + x) * 3;
                let target = (y * place.width + x) * 4;
                pixels[source..source + 3]
                    == [expected[target + 2], expected[target + 1], expected[target]]
            })
        });
        if matches {
            rendered = true;
            break;
        }
    }
    assert!(
        rendered,
        "chooser did not present over the native compositor within {TIMEOUT:?}"
    );

    stop.store(true, Ordering::Relaxed);
    let _ = client.join().unwrap();
    compositor.stop();
}
