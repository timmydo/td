use super::*;
use std::io::{BufRead, BufReader};
use std::sync::mpsc;

const FRAME_BYTES: usize = 800 * 600 * 3;

struct Compositor {
    child: Child,
    directory: PathBuf,
    session: String,
    action: u64,
    output: Option<JoinHandle<()>>,
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
        let binary = PathBuf::from(std::env::var_os("TD_TEST_COMPOSITOR").expect(
            "set TD_TEST_COMPOSITOR to an explicitly built td-compositor; see README",
        ));
        assert!(
            binary.is_absolute(),
            "compositor test tool must be an absolute path"
        );
        let session_dir = directory.0.join("session");
        let child = Command::new(binary)
            .args(["headless", "--session-dir"])
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

    fn key(&mut self, key: u32, down: bool) {
        let action = self.action + 1;
        let line = format!(
            "key {} {action} {key} {}",
            self.session,
            if down { "down" } else { "up" }
        );
        let expected = format!(
            "ok\ntd-action-v1 session={} action={action}\n",
            self.session
        );
        assert_eq!(self.request(&line, 1024), expected.as_bytes());
        self.action = action;
    }

    fn chord(&mut self, modifier: Option<u32>, key: u32) {
        if let Some(modifier) = modifier {
            self.key(modifier, true);
        }
        self.key(key, true);
        self.key(key, false);
        if let Some(modifier) = modifier {
            self.key(modifier, false);
        }
    }

    fn window(&self) -> String {
        let layout = self.request("layout", 65536);
        let text = std::str::from_utf8(&layout).unwrap();
        let mut windows = text
            .lines()
            .filter_map(|line| line.strip_prefix("window id="));
        let window = windows
            .next()
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        assert!(number(window.strip_prefix('@').unwrap()).unwrap() > 0);
        assert!(
            windows.next().is_none(),
            "fixture expects exactly one editor window"
        );
        window
    }

    fn observe(&self, window: &str) -> Observation {
        let request = format!("observe-client {} {window}", self.session);
        observation(&self.request(&request, 1024), &self.session, window).unwrap()
    }

    fn rendered_text(
        &self,
        editor: &mut EditorProcess,
        window: &str,
        revision: u64,
        after: Observation,
        text: &str,
        caret: usize,
    ) {
        let state = editor.ok("state");
        let generation = field(&state, "window-generation").unwrap();
        let frame = editor.ok(&format!("wait-frame\t{generation}"));
        let fields: Vec<_> = frame.split(',').collect();
        assert_eq!(
            &fields[2..],
            &["1", &revision.to_string(), "800", "576", "1"]
        );
        let expected = text_pixels(text);
        let width = text.len() * 8;
        assert!(caret < text.len());
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(
                Instant::now() < deadline,
                "editor did not produce correlated text pixels"
            );
            let first = self.observe(window);
            if !first.current || first.commit <= after.commit {
                continue;
            }
            assert_eq!(first.client, after.client);
            let capture = self.request("capture", FRAME_BYTES + 128);
            let (output, pixels) = ppm(&capture, &self.session).unwrap();
            let second = self.observe(window);
            if !second.current || second.commit != first.commit {
                continue;
            }
            assert_eq!(second.client, first.client);
            assert!(output > first.output && output <= second.output);
            // Desktop bar stays 24px high in fullscreen; the document starts
            // at surface (8,48), hence output (8,72). Ignore only the
            // one-pixel caret column so blinking cannot hide a text change.
            let equal = (0..16).all(|y| {
                (0..width).all(|x| {
                    if x == caret * 8 {
                        return true;
                    }
                    let source = ((y + 72) * 800 + x + 8) * 3;
                    let target = (y * width + x) * 4;
                    pixels[source..source + 3]
                        == [expected[target + 2], expected[target + 1], expected[target]]
                })
            });
            if equal {
                return;
            }
        }
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

fn text_pixels(text: &str) -> Vec<u8> {
    use td_editor::render::{Draw, Geometry, GlyphStyle, INK, PAPER, Primitive, Raster, Scale};
    assert!(text.is_ascii() && !text.is_empty() && text.len() <= 32);
    let font = td_editor::font::pinned().unwrap();
    let width = text.len() * 8;
    let geometry = Geometry::new(width, 16, Scale::new(1).unwrap()).unwrap();
    let mut pixels = PAPER.to_le_bytes().repeat(width * 16);
    let mut raster = Raster::new(&mut pixels, &font, geometry, width * 4).unwrap();
    for (column, scalar) in text.chars().enumerate() {
        raster.draw(Draw {
            clip: geometry.bounds(),
            primitive: Primitive::Glyph {
                x: (column * 8) as i64,
                y: 0,
                scalar,
                style: GlyphStyle::medium(INK, PAPER),
            },
        });
    }
    pixels
}

fn keyboard_profile(profile: &str) {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"one\n").unwrap();
    std::fs::write(&dictionary, b"one\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor =
        EditorProcess::start_with_profile(&directory, &display, &file, &dictionary, profile);
    editor.wait_keyboard(profile);
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    let before = compositor.observe(&window);
    compositor.chord(Some(KEY_LEFT_SHIFT), KEY_A);
    editor.wait_tab(1, "Aone\n");
    compositor.rendered_text(&mut editor, &window, 1, before, "Aone", 1);
    let before = compositor.observe(&window);
    compositor.chord(None, KEY_B);
    editor.wait_tab(2, "Abone\n");
    compositor.rendered_text(&mut editor, &window, 2, before, "Abone", 2);
    let before = compositor.observe(&window);
    compositor.chord(
        Some(KEY_LEFT_CTRL),
        if profile == "windows" {
            KEY_Z
        } else {
            KEY_SLASH
        },
    );
    editor.wait_tab(3, "Aone\n");
    compositor.rendered_text(&mut editor, &window, 3, before, "Aone", 1);
    editor.job("save\t1\t3");
    assert_eq!(std::fs::read(&file).unwrap(), b"Aone\n");
    editor.quit();
    compositor.child.stdin.take();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = compositor.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "compositor owner-EOF deadline");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!compositor.directory.exists());
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_windows_keyboard() {
    keyboard_profile("windows");
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_emacs_keyboard() {
    keyboard_profile("emacs");
}

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
        format!("ok\nP6\n# td-output-v1 session={session} output=4\n800 600\n255\n").into_bytes();
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
