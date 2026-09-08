use super::*;
use std::io::{BufRead, BufReader};
use std::sync::mpsc;

const FRAME_BYTES: usize = 800 * 600 * 3;
const KEY_W: u32 = 17;
const KEY_Y: u32 = 21;
const KEY_G: u32 = 34;
const KEY_C: u32 = 46;
const KEY_LEFT_ALT: u32 = 56;
const KEY_SPACE: u32 = 57;
const KEY_HOME: u32 = 102;
const KEY_RIGHT: u32 = 106;
const KEY_END: u32 = 107;

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
        Self::start_with_clipboard(directory, false)
    }

    fn start_with_clipboard(directory: &Directory, clipboard: bool) -> Self {
        let binary = PathBuf::from(
            std::env::var_os("TD_TEST_COMPOSITOR")
                .expect("set TD_TEST_COMPOSITOR to an explicitly built td-compositor; see README"),
        );
        assert!(
            binary.is_absolute(),
            "compositor test tool must be an absolute path"
        );
        let session_dir = directory.0.join("session");
        let mut command = Command::new(binary);
        if clipboard {
            command.args(["headless", "--clipboard-control", "enabled"]);
        } else {
            command.arg("headless");
        }
        let child = command
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
        // Timestamps follow the receipt counter; callers do not send action IDs.
        let time = self.action + 1;
        let line = format!(
            "key {} {time} {key} {}",
            self.session,
            if down { "down" } else { "up" }
        );
        self.receipt(&line);
    }

    fn receipt(&mut self, line: &str) {
        let action = self.action + 1;
        let expected = format!(
            "ok\ntd-action-v1 session={} action={action}\n",
            self.session
        );
        assert_eq!(self.request(line, 1024), expected.as_bytes());
        self.action = action;
    }

    fn pointer(&mut self, x: u32, y: u32, buttons: u8) {
        self.pointer_frame(x, y, buttons, 0, 0);
    }

    fn pointer_frame(&mut self, x: u32, y: u32, buttons: u8, vertical: i32, horizontal: i32) {
        let time = self.action + 1;
        self.receipt(&format!(
            "pointer {} {time} {x} {y} {buttons} {vertical} {horizontal}",
            self.session
        ));
    }

    fn click(&mut self, x: u32, y: u32) {
        self.pointer(x, y, 1);
        self.pointer(x, y, 0);
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
        let windows = self.windows();
        assert_eq!(
            windows.len(),
            1,
            "fixture expects exactly one editor window"
        );
        windows.into_iter().next().unwrap()
    }

    fn windows(&self) -> Vec<String> {
        let layout = self.request("layout", 65536);
        let text = std::str::from_utf8(&layout).unwrap();
        text.lines()
            .filter_map(|line| line.strip_prefix("window id="))
            .map(|line| {
                let window = line.split_whitespace().next().expect("layout window ID");
                let id = window.strip_prefix('@').expect("layout window ID sigil");
                assert!(number(id).expect("canonical layout window ID") > 0);
                window.to_string()
            })
            .collect()
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
        assert!(caret <= text.len());
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
            // one-pixel caret column when it falls inside the sampled prefix.
            // This compares text pixels, not caret visibility.
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
    compositor.stop();
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
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_pointer_selection_and_menus() {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"one two\n").unwrap();
    std::fs::write(&dictionary, b"one\ntwo\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor = EditorProcess::start(&directory, &display, &file, &dictionary);
    editor.wait_keyboard("windows");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    // Output includes the 24px desktop bar: document origin is (8,72).
    // Literal 8px-cell expectations are independent of editor hit testing.
    compositor.pointer(9, 80, 0);
    editor.wait_field("state", "pointer-ready", "1");
    compositor.pointer(9, 80, 1);
    compositor.pointer(33, 80, 1);
    editor.wait_field("state", "tab", "1,0,0,8,0,3,0,72,0,lf");
    compositor.pointer(33, 80, 0);
    compositor.pointer(65, 80, 0);
    let before = compositor.observe(&window);
    compositor.chord(None, KEY_B);
    // Unheld motion must not extend the selection: replace only "one".
    editor.wait_tab(1, "b two\n");
    compositor.rendered_text(&mut editor, &window, 1, before, "b two", 1);
    compositor.chord(Some(KEY_LEFT_CTRL), KEY_Z);
    editor.wait_tab(2, "one two\n");
    let before = compositor.observe(&window);
    compositor.click(65, 80); // Collapse Undo's restored selection after "two".
    editor.wait_field("state", "tab", "1,2,0,8,7,7,0,72,0,lf");
    compositor.rendered_text(&mut editor, &window, 2, before, "one two", 7);
    compositor.click(68, 32); // Edit header: surface y=8 plus desktop bar.
    editor.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
    compositor.click(68, 252); // Find: panel y=24, zero-based row eight.
    editor.wait_field("prompt-state", "prompt", "find-forward");
    let before = compositor.observe(&window);
    compositor.chord(None, KEY_ESCAPE);
    editor.wait_field("prompt-state", "prompt", "none");
    editor.wait_tab(2, "one two\n");
    compositor.rendered_text(&mut editor, &window, 2, before, "one two", 7);
    editor.job("save\t1\t2");
    assert_eq!(std::fs::read(&file).unwrap(), b"one two\n");
    editor.quit();
    compositor.stop();
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_vertical_wheel_scrolls_without_editing() {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    let text: String = (0..64).map(|row| format!("row{row:02}\n")).collect();
    std::fs::write(&file, &text).unwrap();
    std::fs::write(&dictionary, b"row\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor = EditorProcess::start(&directory, &display, &file, &dictionary);
    editor.wait_keyboard("windows");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    compositor.pointer(400, 80, 0);
    editor.wait_field("state", "pointer-ready", "1");
    for (detents, row, repeat) in [
        (-1, 3, false),
        (-1, 6, false),
        (2, 0, false),
        (-120, 34, true),
        (1, 31, false),
        (120, 0, true),
        (-1, 3, false),
    ] {
        let before = compositor.observe(&window);
        compositor.pointer_frame(400, 80, 0, detents, 0);
        editor.wait_field("state", "view", &format!("1,{row},0,98,31,1,downstream,-"));
        editor.wait_field("state", "tab", "1,0,0,384,0,0,0,72,0,lf");
        assert_eq!(
            editor.ok("text\t1\t0\t0\t384"),
            format!("384\t{}", td_editor::control::hex(text.as_bytes()))
        );
        let prefix = format!("row{row:02}");
        // The caret stays on row zero. Scrolled rows compare every pixel;
        // prefix.len() places the optional one-column mask outside the crop.
        compositor.rendered_text(
            &mut editor,
            &window,
            0,
            before,
            &prefix,
            if row == 0 { 0 } else { prefix.len() },
        );
        if repeat {
            // A clamped no-op owes no redraw. The following inward report
            // proves the resulting viewport without demanding a new frame here.
            compositor.pointer_frame(400, 80, 0, detents, 0);
        }
    }
    assert_eq!(std::fs::read(&file).unwrap(), text.as_bytes());
    editor.quit();
    compositor.stop();
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_horizontal_wheel_respects_wrap_and_clamps_columns() {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    let text = "abcdefghijklmnopqrstuvwxyz".repeat(5);
    std::fs::write(&file, &text).unwrap();
    std::fs::write(&dictionary, b"word\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor = EditorProcess::start(&directory, &display, &file, &dictionary);
    editor.wait_keyboard("windows");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    compositor.pointer(400, 80, 0);
    editor.wait_field("state", "pointer-ready", "1");
    compositor.pointer_frame(400, 80, 0, 0, 120); // Soft Wrap suppresses this.
    compositor.click(140, 32); // Format header.
    editor.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
    // Menu admission follows the wheel frame on the same native pointer stream.
    editor.wait_field("state", "view", "1,0,0,98,31,1,downstream,-");
    compositor.click(140, 60); // Soft Wrap, first row.
    editor.wait_field("state", "view", "1,0,0,98,31,0,downstream,-");
    editor.wait_field("state", "modal", "0,0,0,0,0,0,0,0,0");
    compositor.pointer(400, 80, 0);
    for (columns, left, prefix, repeat) in [
        (1, 3, "defgh", false),
        (120, 33, "hijkl", true),
        (-1, 30, "efghi", false),
        (-120, 0, "abcde", true),
        (1, 3, "defgh", false),
    ] {
        let before = compositor.observe(&window);
        // Both axes in one report: the single logical row cannot scroll down.
        compositor.pointer_frame(400, 80, 0, -1, columns);
        editor.wait_field("state", "view", &format!("1,0,{left},98,31,0,downstream,-"));
        editor.wait_field("state", "tab", "1,0,0,130,0,0,0,72,0,lf");
        assert_eq!(
            editor.ok("text\t1\t0\t0\t130"),
            format!("130\t{}", td_editor::control::hex(text.as_bytes()))
        );
        compositor.rendered_text(
            &mut editor,
            &window,
            0,
            before,
            prefix,
            if left == 0 { 0 } else { prefix.len() },
        );
        if repeat {
            // As above, the following inward report fences this clamped no-op.
            compositor.pointer_frame(400, 80, 0, -1, columns);
        }
    }
    assert_eq!(std::fs::read(&file).unwrap(), text.as_bytes());
    editor.quit();
    compositor.stop();
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_clipboard_transfers_cut_snapshot_between_editors() {
    clipboard_between_editors("windows", ClipboardOperation::Cut);
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_emacs_clipboard_transfers_marked_snapshot_between_editors() {
    clipboard_between_editors("emacs", ClipboardOperation::Cut);
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_windows_copy_preserves_selection_and_transfers_snapshot() {
    clipboard_between_editors("windows", ClipboardOperation::Copy);
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_emacs_copy_preserves_selection_and_transfers_snapshot() {
    clipboard_between_editors("emacs", ClipboardOperation::Copy);
}

enum ClipboardOperation {
    Copy,
    Cut,
}

fn copy_clipboard_text(compositor: &mut Compositor, profile: &str) {
    if profile == "emacs" {
        compositor.chord(Some(KEY_LEFT_ALT), KEY_W);
    } else {
        compositor.chord(Some(KEY_LEFT_CTRL), KEY_C);
    }
}

fn select_clipboard_text(compositor: &mut Compositor, profile: &str) {
    if profile == "emacs" {
        compositor.chord(Some(KEY_LEFT_CTRL), KEY_HOME);
        compositor.chord(Some(KEY_LEFT_CTRL), KEY_SPACE); // Set mark.
        compositor.chord(Some(KEY_LEFT_CTRL), KEY_END); // Extend to document end.
    } else {
        compositor.chord(Some(KEY_LEFT_CTRL), KEY_A);
    }
}

fn clipboard_between_editors(profile: &str, operation: ClipboardOperation) {
    assert!(matches!(profile, "windows" | "emacs"), "unsupported key profile");
    let compositor_directory = Directory::new();
    let source_directory = Directory::new();
    let destination_directory = Directory::new();
    let mut compositor = Compositor::start_with_clipboard(&compositor_directory, true);
    let source_path = source_directory.0.join("source");
    let source_dictionary = source_directory.0.join("dictionary");
    let text = "clip café e\u{301} 🦀\nsecond line\n";
    std::fs::write(&source_path, text).unwrap();
    std::fs::write(&source_dictionary, b"clip\nline\nsecond\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut source = EditorProcess::start_with_profile(
        &source_directory,
        &display,
        &source_path,
        &source_dictionary,
        profile,
    );
    source.wait_keyboard(profile);
    let source_window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    source.wait_field("state", "window", "800,576,1");
    source.rendered_at(800, 576);
    select_clipboard_text(&mut compositor, profile);
    // CONTROL.md tab: ID, revision, dirty, bytes, anchor, caret,
    // auto-fill, fill-column, BOM, ending. Keep the full wire oracle literal.
    let initial_selection = format!("1,0,0,{0},0,{0},0,72,0,lf", text.len());
    source.wait_field("state", "tab", &initial_selection);
    let paste_key = if profile == "emacs" { KEY_Y } else { KEY_V };
    let source_revision = match operation {
        ClipboardOperation::Cut => {
            let cut_key = if profile == "emacs" { KEY_W } else { KEY_X };
            compositor.chord(Some(KEY_LEFT_CTRL), cut_key);
            source.wait_tab(1, "");
            2
        }
        ClipboardOperation::Copy => {
            let offered = td_editor::control::hex(b"Selection offered to clipboard.");
            assert_ne!(
                field(&source.ok("prompt-state"), "notice"),
                Some(offered.as_str()),
                "Copy must start without prior selection-offered feedback"
            );
            copy_clipboard_text(&mut compositor, profile);
            source.wait_field("prompt-state", "notice", &offered);
            source.wait_field("state", "tab", &initial_selection);
            source.wait_tab(0, text);
            assert_eq!(std::fs::read(&source_path).unwrap(), text.as_bytes());
            1
        }
    };
    let before = compositor.observe(&source_window);
    compositor.chord(None, KEY_B);
    source.wait_tab(source_revision, "b");
    compositor.rendered_text(&mut source, &source_window, source_revision, before, "b", 1);
    source.job(&format!("save\t1\t{source_revision}"));
    assert_eq!(std::fs::read(&source_path).unwrap(), b"b");
    let collapsed = format!("1,{source_revision},0,1,1,1,0,72,0,lf");
    source.wait_field("state", "tab", &collapsed);
    let empty_copy = td_editor::control::hex(b"Nothing selected to copy.");
    assert_ne!(
        field(&source.ok("prompt-state"), "notice"),
        Some(empty_copy.as_str())
    );
    copy_clipboard_text(&mut compositor, profile);
    source.wait_field("prompt-state", "notice", &empty_copy);
    source.wait_field("state", "tab", &collapsed);
    source.wait_tab(source_revision, "b");
    assert_eq!(std::fs::read(&source_path).unwrap(), b"b");
    source.wait_field("clipboard-state", "device", "1");
    source.wait_field("clipboard-state", "source-bytes", &text.len().to_string());
    // The destination must still receive the original offer, not empty text.
    // Reveal tiling before mapping the destination. The reply fences the
    // compositor layout change; no intermediate source frame is sampled.
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");

    let destination_path = destination_directory.0.join("destination");
    let destination_dictionary = destination_directory.0.join("dictionary");
    std::fs::write(&destination_path, b"").unwrap();
    std::fs::write(&destination_dictionary, b"clip\nline\nsecond\n").unwrap();
    let mut destination = EditorProcess::start_with_profile(
        &destination_directory,
        &display,
        &destination_path,
        &destination_dictionary,
        profile,
    );
    destination.wait_keyboard(profile);
    destination.wait_field("state", "focus", "1");
    source.wait_field("state", "focus", "0");
    source.wait_field("clipboard-state", "focus", "0");
    source.wait_field("clipboard-state", "source-bytes", &text.len().to_string());
    destination.wait_field("clipboard-state", "selection", "utf8");
    destination.wait_field("clipboard-state", "source-bytes", "-");
    let windows = compositor.windows();
    assert_eq!(windows.len(), 2);
    assert!(windows.contains(&source_window));
    let destination_window = windows.iter().find(|id| **id != source_window).unwrap();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    destination.wait_field("state", "window", "800,576,1");
    destination.rendered_at(800, 576);
    destination.wait_tab(0, ""); // An offer is not an insertion.
    let before_paste = compositor.observe(destination_window);
    assert_ne!(before_paste.client, before.client);
    let hold_reply = |state: &str| format!(
        "ok\ntd-clipboard-v1 session={} hold=1 window={destination_window} state={state}\n",
        compositor.session,
    );
    // Pin the first hold in this fresh compositor; do not accept arbitrary IDs.
    assert_eq!(compositor.request(&format!(
        "clipboard-arm {} {destination_window}", compositor.session,
    ), 1024), hold_reply("armed").as_bytes());
    let held = hold_reply("held");
    let armed = hold_reply("armed");
    let released = hold_reply("released");
    let paste_started = Instant::now();
    compositor.chord(Some(KEY_LEFT_CTRL), paste_key);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let reply = compositor.request(&format!("clipboard-status {} 1", compositor.session), 1024);
        if reply == held.as_bytes() {
            break;
        }
        assert_eq!(reply, armed.as_bytes(), "hold failed before receiving the transfer");
        assert!(
            Instant::now() < deadline,
            "native Paste never reached the hold: {}",
            String::from_utf8_lossy(&reply)
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    destination.wait_field("clipboard-state", "incoming", "1");
    source.wait_field("clipboard-state", "outgoing", "0");
    destination.wait_field("state", "tab", "1,0,0,0,0,0,0,72,0,lf");
    destination.wait_tab(0, "");
    let pasting = td_editor::control::hex(b"Pasting UTF-8 text; Escape cancels.");
    destination.wait_field("prompt-state", "notice", &pasting);
    if profile == "emacs" {
        compositor.chord(Some(KEY_LEFT_CTRL), KEY_G);
    } else {
        compositor.chord(None, KEY_ESCAPE);
    }
    destination.wait_field("clipboard-state", "incoming", "0");
    // A timeout followed by Cancel could clear both fields too. Finish
    // within four seconds measured before Paste, below its five-second
    // reader deadline, or fail closed even if all state assertions pass.
    destination.wait_field("prompt-state", "notice", "-");
    assert!(
        paste_started.elapsed() < Duration::from_secs(4),
        "native Cancel exceeded its evidence budget"
    );
    destination.wait_field("state", "tab", "1,0,0,0,0,0,0,72,0,lf");
    destination.wait_tab(0, "");
    compositor.rendered_text(&mut destination, destination_window, 0, before_paste, " ", 0);
    let send_failed = b"Clipboard send failed:";
    let prior_notice = source.ok("prompt-state");
    let prior_notice = field(&prior_notice, "notice").unwrap();
    assert!(
        prior_notice == "-"
            || !td_editor::control::unhex(prior_notice).unwrap().starts_with(send_failed)
    );
    let release_started = Instant::now();
    assert_eq!(compositor.request(&format!(
        "clipboard-release {} 1", compositor.session,
    ), 1024), released.as_bytes());
    // Fence source processing, not just compositor queue admission or a
    // transient outgoing=0 observed before DataSourceSend reached it.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let reply = source.ok("prompt-state");
        let notice = field(&reply, "notice").unwrap();
        if notice != "-" && td_editor::control::unhex(notice).unwrap().starts_with(send_failed) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "source did not observe the cancelled receiver: {reply}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    source.wait_field("clipboard-state", "outgoing", "0");
    assert!(
        release_started.elapsed() < Duration::from_secs(4),
        "source failure exceeded its evidence budget"
    );
    destination.wait_field("clipboard-state", "incoming", "0");
    destination.wait_field("state", "tab", "1,0,0,0,0,0,0,72,0,lf");
    destination.wait_tab(0, "");
    source.wait_tab(source_revision, "b");
    source.wait_field("state", "tab", &collapsed);
    assert_eq!(std::fs::read(&source_path).unwrap(), b"b");
    assert_eq!(std::fs::read(&destination_path).unwrap(), b"");
    // A second held Paste loses focus before any source send. Compositor
    // invalidation closes its endpoint, so EOF may race the keyboard leave;
    // this checks the settled native state, not which cancellation wins.
    let hold_id = 2;
    let hold_reply = |state: &str| format!(
        "ok\ntd-clipboard-v1 session={} hold={hold_id} window={destination_window} state={state}\n",
        compositor.session,
    );
    let armed = hold_reply("armed");
    assert_eq!(compositor.request(&format!(
        "clipboard-arm {} {destination_window}", compositor.session,
    ), 1024), armed.as_bytes());
    let held = hold_reply("held");
    let invalidated = hold_reply("invalidated");
    let focus_paste_started = Instant::now();
    compositor.chord(Some(KEY_LEFT_CTRL), paste_key);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let reply = compositor.request(
            &format!("clipboard-status {} {hold_id}", compositor.session), 1024,
        );
        if reply == held.as_bytes() {
            break;
        }
        assert_eq!(reply, armed.as_bytes(), "second hold failed before receive");
        assert!(
            Instant::now() < deadline,
            "second native Paste never reached the hold"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    destination.wait_field("clipboard-state", "incoming", "1");
    destination.wait_field("prompt-state", "notice", &pasting);
    let before_focus = compositor.observe(destination_window);
    assert_eq!(
        compositor.request(&format!("focus {source_window}"), 1024), b"ok\n",
    );
    destination.wait_field("state", "focus", "0");
    destination.wait_field("clipboard-state", "focus", "0");
    destination.wait_field("clipboard-state", "selection", "none");
    destination.wait_field("clipboard-state", "incoming", "0");
    assert_ne!(
        field(&destination.ok("prompt-state"), "notice"), Some(pasting.as_str()),
    );
    destination.wait_field("state", "tab", "1,0,0,0,0,0,0,72,0,lf");
    destination.wait_tab(0, "");
    source.wait_field("state", "focus", "1");
    source.wait_field("state", "tab", &collapsed);
    source.wait_tab(source_revision, "b");
    assert_eq!(compositor.request(&format!(
        "clipboard-status {} {hold_id}", compositor.session,
    ), 1024), invalidated.as_bytes());
    assert_eq!(compositor.request(&format!(
        "clipboard-release {} {hold_id}", compositor.session,
    ), 1024), b"unavailable clipboard hold has no releasable transfer\n");
    assert_eq!(std::fs::read(&source_path).unwrap(), b"b");
    assert_eq!(std::fs::read(&destination_path).unwrap(), b"");
    assert!(
        focus_paste_started.elapsed() < Duration::from_secs(4),
        "focus loss exceeded its transfer evidence budget"
    );
    assert_eq!(
        compositor.request(&format!("focus {destination_window}"), 1024), b"ok\n",
    );
    destination.wait_field("state", "focus", "1");
    destination.wait_field("clipboard-state", "selection", "utf8");
    // Named focus reveals the tiled layout; restore the capture geometry.
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    destination.wait_field("state", "window", "800,576,1");
    destination.wait_field("state", "tab", "1,0,0,0,0,0,0,72,0,lf");
    source.wait_field("state", "focus", "0");
    compositor.rendered_text(
        &mut destination, destination_window, 0, before_focus, " ", 0,
    );
    // Returning focus never revives the old descriptor or its hold identity.
    assert_eq!(compositor.request(&format!(
        "clipboard-status {} {hold_id}", compositor.session,
    ), 1024), invalidated.as_bytes());
    let before_paste = compositor.observe(destination_window);
    // A fresh Paste after cancellation must still consume the original offer.
    compositor.chord(Some(KEY_LEFT_CTRL), paste_key);
    destination.wait_tab(1, text);
    destination.wait_field("clipboard-state", "incoming", "0");
    source.wait_field("clipboard-state", "outgoing", "0");
    // The ASCII prefix proves transported pixels; full UTF-8 is checked above.
    // The caret is on the final empty line; mask column 4 is outside the crop.
    compositor.rendered_text(
        &mut destination,
        destination_window,
        1,
        before_paste,
        "clip",
        4,
    );
    destination.job("save\t1\t1");
    assert_eq!(std::fs::read(&destination_path).unwrap(), text.as_bytes());
    source.wait_tab(source_revision, "b");
    let saved_destination = format!("1,1,0,{0},{0},{0},0,72,0,lf", text.len());
    destination.wait_field("state", "tab", &saved_destination);
    source.wait_field("state", "tab", &collapsed);
    let hold_id = 3;
    let hold_reply = |state: &str| format!(
        "ok\ntd-clipboard-v1 session={} hold={hold_id} window={destination_window} state={state}\n",
        compositor.session,
    );
    let armed = hold_reply("armed");
    assert_eq!(compositor.request(&format!(
        "clipboard-arm {} {destination_window}", compositor.session,
    ), 1024), armed.as_bytes());
    let held = hold_reply("held");
    let invalidated = hold_reply("invalidated");
    let owner_exit_paste_started = Instant::now();
    compositor.chord(Some(KEY_LEFT_CTRL), paste_key);
    let deadline = Instant::now() + TIMEOUT;
    let status_command = format!("clipboard-status {} {hold_id}", compositor.session);
    loop {
        let reply = compositor.request(&status_command, 1024);
        if reply == held.as_bytes() {
            break;
        }
        assert_eq!(
            reply, armed.as_bytes(), "owner-exit hold failed before receive: {}",
            String::from_utf8_lossy(&reply),
        );
        assert!(
            Instant::now() < deadline,
            "owner-exit Paste never reached the hold"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    destination.wait_field("clipboard-state", "incoming", "1");
    destination.wait_field("prompt-state", "notice", &pasting);
    destination.wait_field("state", "tab", &saved_destination);
    destination.wait_tab(1, text);
    source.wait_field("clipboard-state", "outgoing", "0");
    source.quit();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = compositor.windows();
        assert!(
            remaining.contains(destination_window),
            "destination window exited prematurely: {remaining:?}"
        );
        if remaining.as_slice() == std::slice::from_ref(destination_window) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "source window still live: {remaining:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    destination.wait_field("state", "focus", "1");
    destination.wait_field("clipboard-state", "selection", "none");
    destination.wait_field("clipboard-state", "incoming", "0");
    destination.wait_field("state", "tab", &saved_destination);
    destination.wait_tab(1, text);
    assert_eq!(compositor.request(&status_command, 1024), invalidated.as_bytes());
    assert_eq!(compositor.request(&format!(
        "clipboard-release {} {hold_id}", compositor.session,
    ), 1024), b"unavailable clipboard hold has no releasable transfer\n");
    assert_eq!(std::fs::read(&destination_path).unwrap(), text.as_bytes());
    assert_eq!(std::fs::read(&source_path).unwrap(), b"b");
    assert!(
        owner_exit_paste_started.elapsed() < Duration::from_secs(4),
        "owner exit exceeded its transfer evidence budget"
    );
    select_clipboard_text(&mut compositor, profile);
    destination.wait_field("clipboard-state", "selection", "none");
    let selected = format!("1,1,0,{0},0,{0},0,72,0,lf", text.len());
    destination.wait_field("state", "tab", &selected);
    let no_offer = td_editor::control::hex(b"Clipboard has no supported UTF-8 text offer.");
    assert_ne!(
        field(&destination.ok("prompt-state"), "notice"),
        Some(no_offer.as_str())
    );
    compositor.chord(Some(KEY_LEFT_CTRL), paste_key);
    // Pin the native refusal, not just unchanged text: stale identical data
    // could replace the selection without visibly changing its bytes.
    destination.wait_field("prompt-state", "notice", &no_offer);
    destination.wait_field("state", "tab", &selected);
    destination.wait_tab(1, text);
    let before_collapse = compositor.observe(destination_window);
    if profile == "emacs" {
        compositor.chord(Some(KEY_LEFT_CTRL), KEY_G); // Deactivate and collapse mark.
        destination.wait_field("prompt-state", "notice", "-");
    } else {
        compositor.chord(None, KEY_RIGHT); // Collapse to the selection end.
    }
    destination.wait_field(
        "state",
        "tab",
        &format!("1,1,0,{0},{0},{0},0,72,0,lf", text.len()),
    );
    if profile == "windows" {
        destination.wait_field("prompt-state", "notice", &no_offer);
    }
    compositor.rendered_text(
        &mut destination,
        destination_window,
        1,
        before_collapse,
        "clip",
        4,
    );
    destination.wait_tab(1, text);
    assert_eq!(std::fs::read(&destination_path).unwrap(), text.as_bytes());
    assert_eq!(std::fs::read(&source_path).unwrap(), b"b");
    destination.quit();
    compositor.stop();
}

#[test]
#[ignore = "requires an explicitly built TD_TEST_COMPOSITOR; ready runs this case"]
fn native_control_edit_spelling_save_and_dirty_close() {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("-draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"\xef\xbb\xbfone\r\nwrng\r\n").unwrap();
    std::fs::write(&dictionary, b"one\nwarm\nwrong\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor = EditorProcess::start(&directory, &display, &file, &dictionary);
    editor.wait_keyboard("windows");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    let before = compositor.observe(&window);
    control_edit_jobs_and_close_dialogs(&mut editor, &file);
    // Caret is on the final empty line, outside the first-row prefix.
    compositor.rendered_text(&mut editor, &window, 4, before, "warm", 4);
    assert_eq!(std::fs::read(&file).unwrap(), b"\xef\xbb\xbfwarm one\r\nwrong\r\n");
    editor.quit();
    compositor.stop();
}

#[test]
#[ignore = "requires an explicitly built TD_TEST_COMPOSITOR; ready runs this case"]
fn native_menu_prompts_and_fill_column() {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"one wrng\n").unwrap();
    std::fs::write(&dictionary, b"one\nwrong\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor = EditorProcess::start(&directory, &display, &file, &dictionary);
    editor.wait_keyboard("windows");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    compositor.pointer(9, 80, 0);
    editor.wait_field("state", "pointer-ready", "1");
    let before = compositor.observe(&window);
    control_menu_prompts_and_fill(&mut editor, &file, true, |process, _, group, row| {
        // Literal output coordinates include the 24px desktop bar.
        // Native clicks have no revision field; the prompt answers pin it.
        let (x, prompt) = match (group, row) {
            (1, 8) => (68, "find-forward"),
            (1, 11) => (68, "replace"),
            (3, 1) => (196, "command"),
            _ => panic!("unexpected shared menu choice"),
        };
        compositor.click(x, 32); // Desktop bar (24) plus header inset (8).
        process.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
        compositor.click(x, 60 + row as u32 * 24); // Two bars plus row center (12).
        process.wait_field("prompt-state", "prompt", prompt);
    });
    let filled = format!("{}word\nword", "word ".repeat(15));
    editor.wait_field("state", "tab", "1,3,0,84,0,0,0,80,0,lf");
    editor.wait_tab(3, &filled);
    editor.wait_field("prompt-state", "prompt", "none");
    compositor.rendered_text(&mut editor, &window, 3, before, "word", 0);
    assert_eq!(std::fs::read(&file).unwrap(), filled.as_bytes());
    editor.quit();
    compositor.stop();
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
