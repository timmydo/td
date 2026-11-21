use super::mock_fetch::MockFetch;

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use td_test_compositor::{remaining, write_until, Compositor, Controls, Directory};
use td_ui::control::{frame, hex, Decoder};

/// Every wait returns as soon as its condition holds. One request is
/// bounded by td-ui's control worker and one capture by the compositor's
/// control exchange, five seconds each; at 3840x2160 a debug build's
/// `frame` takes about a second and a capture two and a half, and on a
/// loaded host either can run out, so the comparison asks both again
/// until this deadline.
const TIMEOUT: Duration = Duration::from_secs(60);
/// The outputs increment 7 measured td-web's pages at, both at scale one,
/// the headless compositor's only scale.
const OUTPUTS: &[(usize, usize)] = &[(1920, 1200), (3840, 2160)];
const KEY_Q: u32 = 16;
const KEY_TAB: u32 = 15;
const KEY_ENTER: u32 = 28;
const KEY_LEFTCTRL: u32 = 29;

/// One td-web window, its control socket in its own runtime directory.
struct Web {
    child: Child,
    log: PathBuf,
    socket: PathBuf,
}

impl Web {
    fn start(runtime: &Directory, display: &Path, address: &str) -> Self {
        let log = runtime.0.join("stderr");
        let socket = runtime.0.join("control");
        let child = Command::new(env!("CARGO_BIN_EXE_td-web"))
            .arg("--control-socket")
            .arg(&socket)
            .arg(address)
            .env_clear()
            .env("WAYLAND_DISPLAY", display)
            .env("XDG_RUNTIME_DIR", &runtime.0)
            .env("XDG_CONFIG_HOME", runtime.0.join("config"))
            .env("HOME", &runtime.0)
            .env("TMPDIR", &runtime.0)
            // The cell face alone, as the seam's frame is painted, so the
            // window's pixels and the seam's can be held equal.
            .env("TD_UI_FACE", "bitmap")
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

    /// One request over the control socket: the reply's fields after the
    /// version and the ID.
    fn request(&self, words: &[&str]) -> Vec<String> {
        match self.try_request(words) {
            Some(fields) => fields,
            None => panic!(
                "{words:?}: no complete reply; td-web said: {}",
                self.stderr()
            ),
        }
    }

    /// `request`, or `None` when the window closed the connection with no
    /// reply, as its control worker does past its deadline.
    fn try_request(&self, words: &[&str]) -> Option<Vec<String>> {
        let deadline = Instant::now() + TIMEOUT;
        let mut stream = UnixStream::connect(&self.socket).unwrap();
        let payload = format!("1\t7\t{}", words.join("\t"));
        write_until(&mut stream, &frame(payload.as_bytes()).unwrap(), deadline).unwrap();
        let mut decoder = Decoder::default();
        let mut chunk = [0u8; 16384];
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
        decoder.payload()?;
        let line = String::from_utf8(decoder.finish().unwrap()).unwrap();
        let fields: Vec<String> = line.split('\t').map(str::to_string).collect();
        assert_eq!(fields[..2], ["1", "7"], "{line}");
        assert_eq!(fields[2], "ok", "{words:?}: {line}");
        Some(fields[3..].to_vec())
    }

    fn state(&self) -> BTreeMap<String, String> {
        self.request(&["state"])
            .into_iter()
            .map(|field| {
                let (key, value) = field.split_once('=').unwrap();
                (key.to_string(), value.to_string())
            })
            .collect()
    }

    /// Waits for the state to hold `facts`.
    fn until(&self, facts: &[(&str, &str)]) -> BTreeMap<String, String> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let state = self.state();
            if facts
                .iter()
                .all(|(key, value)| state.get(*key).map(String::as_str) == Some(*value))
            {
                return state;
            }
            assert!(
                Instant::now() < deadline,
                "{facts:?}: {state:?}; td-web said: {}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The window read back as text.
    fn text(&self) -> String {
        let reply = self.request(&["text"]);
        let bytes = td_ui::control::unhex(&reply[2]).unwrap();
        String::from_utf8(bytes).unwrap()
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
                "td-web did not exit; it said: {}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Web {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// FNV-1a 64, as the seam's `frame` digests a frame's RGB rows.
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

/// The picture's one colour and size.
const BLUE: [u8; 3] = [30, 140, 220];
const PICTURE: (usize, usize) = (24, 16);

/// A page with a heading, prose, a link, a list and a picture its own
/// fetch brings, and the page the link goes to.
fn site() -> Vec<super::mock_fetch::Page> {
    let (w, h) = PICTURE;
    let picture = td_png::encode_rgb8(w, h, &BLUE.repeat(w * h)).unwrap();
    vec![
        (
            "https://example.org/",
            "text/html",
            b"<!doctype html><title>Measured</title>\
              <h1>td-web at an output</h1>\
              <p>A paragraph of prose, wrapped at the pane's width.\
              <p><a href=/next>the next page</a>\
              <ul><li>one<li>two</ul>\
              <p><img src=/blue.png alt=blue> after a picture"
                .to_vec(),
        ),
        ("https://example.org/blue.png", "image/png", picture),
        (
            "https://example.org/next",
            "text/html",
            b"<title>Next</title><p>arrived by the seat".to_vec(),
        ),
    ]
}

/// The window's tile once the output shows what the seam paints: the
/// window paints the frame the seam digests when its picture has been
/// laid out, which `loading=none` does not wait for, and the output
/// shows that frame once the window has committed it, so both are asked
/// until the tile's digest is the seam's and the picture is on it.
fn shown(compositor: &Compositor, web: &Web, place: &td_test_compositor::Placement) -> Vec<u8> {
    let deadline = Instant::now() + TIMEOUT;
    let mut last = String::new();
    loop {
        assert!(
            Instant::now() < deadline,
            "the output never showed the seam's frame with its picture: {last}; td-web said: {}",
            web.stderr()
        );
        let Some(reply) = web.try_request(&["frame"]) else {
            last = "no frame reply".into();
            continue;
        };
        let (w, h) = (
            reply[0].parse::<usize>().unwrap(),
            reply[1].parse::<usize>().unwrap(),
        );
        assert_eq!((w, h, reply[2].as_str()), (place.width, place.height, "1"));
        let digest = u64::from_str_radix(&reply[3], 16).unwrap();
        let tile = compositor.tile(place);
        let blue = tile
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|px| **px == BLUE)
            .count();
        if fnv1a64(&tile) == digest && blue >= PICTURE.0 * PICTURE.1 {
            return tile;
        }
        last = format!(
            "digest {digest:016x}, tile {:016x}, {blue} blue",
            fnv1a64(&tile)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The window at each measured output: it fills the output but for
/// td-compositor's frame, fetches the page and its picture, reads back as
/// its words, and the compositor's capture of it is the seam's frame of
/// it, the picture on it; then the seat's Tab and Return follow the link,
/// the seam steps back and opens, and the seat's Ctrl+Q closes it.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn the_window_at_each_measured_output() {
    for &(width, height) in OUTPUTS {
        let server = Directory::new("td-web-process");
        let mut compositor = Compositor::start_sized(
            &server.0,
            Controls {
                capture: true,
                clipboard: false,
            },
            width,
            height,
        );
        assert_eq!(compositor.size(), (width, height));
        let runtime = Directory::new("td-web-process");
        let service = MockFetch::start(&runtime.0, site());
        let started = Instant::now();
        let web = Web::start(&runtime, &compositor.display(), "https://example.org/");
        let deadline = Instant::now() + TIMEOUT;
        let place = loop {
            if let Some(place) = compositor.focused_placement("td-web") {
                break place;
            }
            assert!(
                Instant::now() < deadline,
                "no td-web window; it said: {}",
                web.stderr()
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let state = web.until(&[
            ("title", "Measured - td-web"),
            ("loading", "none"),
            ("body", "page"),
        ]);
        let loaded = started.elapsed();
        assert_eq!(state["status"], "200", "{state:?}");
        let text = web.text();
        for words in [
            "https://example.org/",
            "td-web at an output",
            "A paragraph of prose",
            "the next page",
            "after a picture",
        ] {
            assert!(text.contains(words), "{width}x{height}: {words}: {text}");
        }
        assert!(
            service
                .asked()
                .iter()
                .any(|url| url == "https://example.org/blue.png"),
            "{:?}",
            service.asked()
        );
        // The output but for the compositor's frame: a few columns and
        // a title's rows.
        assert!(
            place.x + place.width <= width
                && place.y + place.height <= height
                && width - place.width <= 16
                && height - place.height <= 64,
            "the window does not fill the output: {place:?}"
        );
        shown(&compositor, &web, &place);
        eprintln!(
            "td-web-native: {width}x{height}: window {}x{}, state loaded in {} ms",
            place.width,
            place.height,
            loaded.as_millis()
        );
        // The seat's keys: Tab to the link, Return to follow it.
        compositor.tap(KEY_TAB);
        web.until(&[("link", "https://example.org/next")]);
        compositor.tap(KEY_ENTER);
        web.until(&[
            ("title", "Next - td-web"),
            ("loading", "none"),
            ("back", "1"),
        ]);
        assert!(web.text().contains("arrived by the seat"));
        web.request(&["action", "back"]);
        web.until(&[("title", "Measured - td-web"), ("forward", "1")]);
        let open = hex(b"https://example.org/next");
        web.request(&["action", "open", &open]);
        web.until(&[("title", "Next - td-web"), ("loading", "none")]);
        compositor.chord(&[KEY_LEFTCTRL], KEY_Q);
        assert!(web.finish(), "td-web did not close well");
        compositor.stop();
    }
}
