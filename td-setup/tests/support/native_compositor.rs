//! The real headless `td-compositor` as a render oracle for the td-setup
//! installer client. A generic `Compositor` (the same control protocol the
//! td-editor native harness drives) launches the compositor from the
//! `TD_TEST_COMPOSITOR` binary, and `SetupProcess` launches the installer as
//! an ordinary client against its Wayland socket. The one case proves the
//! whole live path the wire tests cannot: the client connects, binds, obeys
//! the compositor's configure, accepts a keyboard press, and presents the
//! welcome and unavailable destination pages pixel for pixel.

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

/// The td-setup installer launched as an ordinary Wayland client against the
/// compositor's socket. It maps a toplevel and reads the seat's keyboard.
struct SetupProcess {
    child: Child,
    log: PathBuf,
}
impl SetupProcess {
    fn start(directory: &Directory, display: &Path) -> Self {
        let log = directory.0.join("stderr");
        let child = Command::new(env!("CARGO_BIN_EXE_td-setup"))
            .env_clear()
            .env("WAYLAND_DISPLAY", display)
            .env("XDG_RUNTIME_DIR", &directory.0)
            .env("XDG_CONFIG_HOME", directory.0.join("config"))
            .env("TMPDIR", &directory.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .env("TD_UI_FACE", "bitmap")
            .spawn()
            .unwrap();
        Self { child, log }
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}
impl Drop for SetupProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn installer_navigation_presents_over_the_native_compositor() {
    let compositor_directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let client_directory = Directory::new();
    let client = SetupProcess::start(&client_directory, &compositor.directory.join("wayland-0"));

    // Wait for the client to bind, set its app id, and map its one toplevel;
    // the compositor then reports the tile it composited it into.
    let deadline = Instant::now() + TIMEOUT;
    let place = loop {
        if let Some(place) = compositor.placement("td-setup") {
            break place;
        }
        assert!(
            Instant::now() < deadline,
            "td-setup window never mapped; client stderr: {}",
            client.stderr()
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(
        place.x + place.width <= OUTPUT_WIDTH && place.y + place.height <= OUTPUT_HEIGHT,
        "reported tile {place:?} exceeds the {OUTPUT_WIDTH}x{OUTPUT_HEIGHT} output"
    );

    // The client obeys the compositor's configure and presents the welcome
    // page at the tile's extent, so its captured pixels equal the crate's own
    // `preview` of that same surface. A captured frame may still be the
    // earlier one at the default extent (from before the tile configure),
    // clipped to the tile; the loop waits for the settled frame under a
    // two-observation sandwich around the capture. The render gets its own
    // deadline, not the remainder of the mapping one, and the deadline bounds
    // the whole wait: a match is accepted only within it, so a wrong or
    // never-settling frame fails here rather than passing late. Each poll is
    // spaced so the retry does not busy-spin the compositor and client that
    // still need CPU to settle the frame.
    let expected = td_setup::preview(place.width, place.height, 1).unwrap();
    assert_frame(&compositor, &place, &expected, &client, "welcome");

    // Enter moves to the explicit unavailable state; Escape returns to
    // welcome. The pure state test proves that a second Enter cannot advance
    // without a service.
    let unavailable = unavailable_pixels(place.width, place.height);
    for (step, (time, code, label, frame)) in [
        (1, 28, "destination", unavailable.as_slice()),
        (3, 1, "welcome again", expected.as_slice()),
    ]
    .into_iter()
    .enumerate()
    {
        let down_action = step * 2 + 1;
        let up_action = down_action + 1;
        let request = format!("key {} {time} {code} down", compositor.session);
        let reply = compositor.request(&request, 1024);
        assert_eq!(
            reply,
            format!(
                "ok\ntd-action-v1 session={} action={down_action}\n",
                compositor.session
            )
            .as_bytes(),
            "{label} key receipt"
        );
        let release = format!("key {} {} {code} up", compositor.session, time + 1);
        let reply = compositor.request(&release, 1024);
        assert_eq!(
            reply,
            format!(
                "ok\ntd-action-v1 session={} action={up_action}\n",
                compositor.session
            )
            .as_bytes(),
            "{label} key release"
        );
        assert_frame(&compositor, &place, frame, &client, label);
    }

    drop(client);
    compositor.stop();
}

/// F12 on the seat is the window's theme chord: welcome is painted again
/// in the next theme, pixel for pixel, the choice kept in the program's
/// file, and no page moves; S-F12 keeps no theme.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn f12_on_the_seat_paints_welcome_in_the_next_theme_and_keeps_it() {
    let compositor_directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let client_directory = Directory::new();
    let client = SetupProcess::start(&client_directory, &compositor.directory.join("wayland-0"));
    let file = client_directory.0.join("config/td-setup/theme");
    let deadline = Instant::now() + TIMEOUT;
    let place = loop {
        if let Some(place) = compositor.placement("td-setup") {
            break place;
        }
        assert!(Instant::now() < deadline, "unmapped: {}", client.stderr());
        std::thread::sleep(Duration::from_millis(2));
    };
    let sand = td_setup::preview(place.width, place.height, 1).unwrap();
    assert_frame(&compositor, &place, &sand, &client, "welcome");
    let mut action = 0;
    let mut key = |compositor: &mut Compositor, code: u32, state: &str| {
        action += 1;
        let request = format!("key {} {action} {code} {state}", compositor.session);
        let reply = compositor.request(&request, 1024);
        assert_eq!(
            reply,
            format!(
                "ok\ntd-action-v1 session={} action={action}\n",
                compositor.session
            )
            .as_bytes(),
            "key {code} {state}"
        );
    };
    key(&mut compositor, 88, "down");
    key(&mut compositor, 88, "up");
    let harbor = welcome_pixels(place.width, place.height, &td_ui::theme::HARBOR);
    assert_ne!(harbor, sand);
    assert_frame(&compositor, &place, &harbor, &client, "welcome in harbor");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "harbor\n");
    key(&mut compositor, 42, "down");
    key(&mut compositor, 88, "down");
    key(&mut compositor, 88, "up");
    key(&mut compositor, 42, "up");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "harbor\n");
    assert_frame(&compositor, &place, &harbor, &client, "still harbor");
    drop(client);
    compositor.stop();
}

fn welcome_pixels(width: usize, height: usize, theme: &'static td_ui::theme::Theme) -> Vec<u8> {
    use td_ui::raster::{Raster, Scale, Surface};
    let surface = Surface::new(width, height, Scale::new(1).unwrap()).unwrap();
    let page = td_setup::welcome::Welcome::new(surface).unwrap();
    let font = td_ui::font::pinned().unwrap();
    let mut pixels = vec![0; width * height * 4];
    Raster::new(&mut pixels, &font, surface, width * 4)
        .unwrap()
        .with_theme(theme)
        .paint(&page, surface.bounds())
        .unwrap();
    pixels
}

fn unavailable_pixels(width: usize, height: usize) -> Vec<u8> {
    use td_ui::raster::{Raster, Scale, Surface};
    let surface = Surface::new(width, height, Scale::new(1).unwrap()).unwrap();
    let page = td_setup::destination::DestinationPage::unavailable(surface);
    assert!(
        page.is_some(),
        "destination page does not fit {width}x{height}"
    );
    let page = page.unwrap();
    let font = td_ui::font::pinned().unwrap();
    let mut pixels = vec![0; width * height * 4];
    Raster::new(&mut pixels, &font, surface, width * 4)
        .unwrap()
        .paint(&page, surface.bounds())
        .unwrap();
    pixels
}

fn assert_frame(
    compositor: &Compositor,
    place: &Placement,
    expected: &[u8],
    client: &SetupProcess,
    label: &str,
) {
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
        "td-setup did not present {label} within {TIMEOUT:?}; client stderr: {}",
        client.stderr()
    );
}
