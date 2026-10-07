//! The real headless `td-compositor` as a render and transport oracle for
//! the td-photo window. td's shared native harness (`td-test-compositor`)
//! launches the compositor from the `TD_TEST_COMPOSITOR` binary, and `PhotoProcess` launches the window
//! as an ordinary client against its Wayland socket, serving the seam on a
//! control socket of its own. The one case proves the live path the replay
//! cannot: the window binds, obeys the configure, presents the roll, answers
//! the socket in the replay's vocabulary, holds `wait-idle` until the frame
//! on screen is the model's, writes a flag through the sidecar and shows it,
//! takes a click and a key from the seat, and closes on `quit`; its captured
//! pixels equal the crate's own `--preview` of the same roll at the same
//! size, before the flag and after.

use super::*;

use td_test_compositor::{Compositor, Controls, OUTPUT_HEIGHT, OUTPUT_WIDTH};

/// evdev's KEY_END, the chord `End`, which `last` binds: the last photo from
/// anywhere, so a press repeated until one lands moves the cursor once.
const KEY_END: u32 = 107;

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
            .env("XDG_CONFIG_HOME", directory.0.join("config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .env("TD_UI_FACE", "bitmap")
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
    let compositor_directory = Directory::new("td-photo-process");
    let mut compositor = Compositor::start(
        &compositor_directory.0,
        Controls {
            capture: true,
            clipboard: false,
        },
    );
    let client_directory = Directory::new("td-photo-process");
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
    let client = PhotoProcess::start(&client_directory, &compositor.display(), &roll);

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

/// F12 on the seat is the window's theme chord: the frame is painted
/// again in the next theme and the choice kept in the program's file,
/// while the session hears nothing; `wait-idle` waits for that frame.
/// S-F12 is the session's, and keeps no theme.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn f12_on_the_seat_paints_and_keeps_the_next_theme() {
    let compositor_directory = Directory::new("td-photo-process");
    let mut compositor = Compositor::start(
        &compositor_directory.0,
        Controls {
            capture: true,
            clipboard: false,
        },
    );
    let client_directory = Directory::new("td-photo-process");
    let roll = client_directory.0.join("roll");
    std::fs::create_dir(&roll).unwrap();
    std::fs::write(roll.join("DSC_0001.NEF"), b"not really a nef").unwrap();
    let client = PhotoProcess::start(&client_directory, &compositor.display(), &roll);
    let file = client_directory.0.join("config/td-photo/theme");
    let deadline = Instant::now() + TIMEOUT;
    let place = loop {
        if let Some(place) = compositor.placement("td-photo") {
            break place;
        }
        assert!(Instant::now() < deadline, "unmapped: {}", client.stderr());
        std::thread::sleep(Duration::from_millis(2));
    };
    client.settle(1);
    let state = client.request(2, &["state"]);
    let sand = compositor.tile(&place);
    assert_eq!(
        sand,
        preview(&client_directory, place.width, place.height, &roll)
    );
    compositor.key(88, true);
    compositor.key(88, false);
    // The receipt says the compositor queued the press, not that the
    // window took it: the kept file says it did, and only then does
    // `wait-idle` speak for the frame after it.
    let deadline = Instant::now() + TIMEOUT;
    while std::fs::read_to_string(&file).ok().as_deref() != Some("harbor\n") {
        assert!(Instant::now() < deadline, "unkept: {}", client.stderr());
        std::thread::sleep(Duration::from_millis(2));
    }
    client.settle(3);
    let harbor = compositor.tile(&place);
    assert_ne!(harbor, sand);
    let paper = td_ui::theme::HARBOR.map(td_ui::raster::PAPER).to_be_bytes();
    assert!(harbor
        .as_chunks::<3>()
        .0
        .iter()
        .any(|p| p[..] == paper[1..]));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "harbor\n");
    assert_eq!(
        client.request(4, &["state"]),
        state,
        "F12 is not the session's"
    );
    compositor.key(42, true);
    compositor.key(88, true);
    compositor.key(88, false);
    compositor.key(42, false);
    client.settle(5);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "harbor\n");
    assert_eq!(compositor.tile(&place), harbor);
}

/// F1 on the seat is the window's key list: the frame is painted again
/// with the list's title bar over it while the session hears nothing; a
/// key the session binds is the list's while it is open, a reading key
/// scrolls it, and Escape closes it, the frame the session's again. A
/// click on the mode strip's Help opens it as F1 does, and a click
/// anywhere closes it, stopping a reading key's repeat with it.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn f1_on_the_seat_shows_the_key_list_over_the_frame() {
    const KEY_ESCAPE: u32 = 1;
    const KEY_P: u32 = 25;
    const KEY_F1: u32 = 59;
    const KEY_DOWN: u32 = 108;
    let compositor_directory = Directory::new("td-photo-process");
    let mut compositor = Compositor::start(
        &compositor_directory.0,
        Controls {
            capture: true,
            clipboard: false,
        },
    );
    let client_directory = Directory::new("td-photo-process");
    let roll = client_directory.0.join("roll");
    std::fs::create_dir(&roll).unwrap();
    // Rows of photos, so a held Down that reached the session would move
    // its cursor.
    for n in 1..=12 {
        std::fs::write(roll.join(format!("DSC_{n:04}.NEF")), b"not really a nef").unwrap();
    }
    let client = PhotoProcess::start(&client_directory, &compositor.display(), &roll);
    let deadline = Instant::now() + TIMEOUT;
    let place = loop {
        if let Some(place) = compositor.placement("td-photo") {
            break place;
        }
        assert!(Instant::now() < deadline, "unmapped: {}", client.stderr());
        std::thread::sleep(Duration::from_millis(2));
    };
    client.settle(1);
    let state = client.request(2, &["state"]);
    let frame = compositor.tile(&place);
    // The receipt says the compositor queued a press, not that the window
    // took it: a changed frame does, and `wait-idle` speaks for it after.
    let changed = |compositor: &Compositor, from: &[u8]| {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let tile = compositor.tile(&place);
            if tile != from {
                break tile;
            }
            assert!(Instant::now() < deadline, "unchanged: {}", client.stderr());
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    compositor.key(KEY_F1, true);
    compositor.key(KEY_F1, false);
    changed(&compositor, &frame);
    client.settle(3);
    let open = compositor.tile(&place);
    let surface =
        td_ui::raster::Surface::new(place.width, place.height, td_ui::raster::Scale::default())
            .unwrap();
    let title = td_ui::keys::Panel::new(surface).unwrap().title;
    let selected = td_ui::raster::SELECTED.to_be_bytes();
    let (x0, y0) = (title.x as usize, title.y as usize);
    let (width, height) = (title.width as usize, title.height as usize);
    // How much of the title bar is in the selection's colour: over half,
    // the list's.
    let filled = |tile: &[u8]| {
        (y0..y0 + height)
            .flat_map(|y| (x0..x0 + width).map(move |x| (y * place.width + x) * 3))
            .filter(|at| tile[*at..*at + 3] == selected[1..])
            .count()
    };
    let shown = |tile: &[u8]| filled(tile) > width * height / 2;
    assert!(
        shown(&open),
        "the title bar is not the list's: {} of {}",
        filled(&open),
        width * height
    );
    assert_eq!(
        client.request(4, &["state"]),
        state,
        "F1 is not the session's"
    );
    // `p` would pick the photo; the list keeps it. `End` then scrolls the
    // list, a frame that says the window took both.
    compositor.key(KEY_P, true);
    compositor.key(KEY_P, false);
    compositor.key(KEY_END, true);
    compositor.key(KEY_END, false);
    changed(&compositor, &open);
    client.settle(5);
    assert_eq!(
        client.request(6, &["state"]),
        state,
        "p reached the session"
    );
    compositor.key(KEY_ESCAPE, true);
    compositor.key(KEY_ESCAPE, false);
    let deadline = Instant::now() + TIMEOUT;
    while compositor.tile(&place) != frame {
        assert!(Instant::now() < deadline, "still open: {}", client.stderr());
        std::thread::sleep(Duration::from_millis(2));
    }
    client.settle(7);
    assert_eq!(compositor.tile(&place), frame);
    assert_eq!(
        client.request(8, &["state"]),
        state,
        "Escape is the list's, not the session's"
    );
    // The mode strip's Help, pressed from the seat, opens the list too; a
    // tile that changes is not proof, as the seat draws its cursor over
    // it, so the title bar is waited for. The session hears neither the
    // press nor its release.
    let help = td_photo::ui::Controller::new(surface)
        .help_button()
        .expect("Help on the tile's mode strip");
    compositor.click(
        place.x + help.x as usize + help.width as usize / 2,
        place.y + help.y as usize + help.height as usize / 2,
    );
    let deadline = Instant::now() + TIMEOUT;
    while !shown(&compositor.tile(&place)) {
        assert!(
            Instant::now() < deadline,
            "Help did not open: {}",
            client.stderr()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    client.settle(9);
    assert_eq!(
        client.request(10, &["state"]),
        state,
        "Help is the window's, not the session's"
    );
    // Down held scrolls the list and repeats; a click then closes it,
    // Roll Selection under it reached by neither the press nor its
    // release, and the repeat stops with it, so Down held past the
    // repeat's delay moves no cursor.
    let scrolled = compositor.tile(&place);
    compositor.key(KEY_DOWN, true);
    changed(&compositor, &scrolled);
    compositor.click(place.x + 40, place.y + 12);
    compositor.pointer(0, 0, 0);
    std::thread::sleep(Duration::from_millis(1500));
    compositor.key(KEY_DOWN, false);
    let deadline = Instant::now() + TIMEOUT;
    while shown(&compositor.tile(&place)) {
        assert!(
            Instant::now() < deadline,
            "a click did not close: {}",
            client.stderr()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    client.settle(11);
    assert_eq!(
        client.request(12, &["state"]),
        state,
        "the click, its release or Down's repeat reached the session"
    );
    let deadline = Instant::now() + TIMEOUT;
    while compositor.tile(&place) != frame {
        assert!(
            Instant::now() < deadline,
            "not the frame before: {}",
            client.stderr()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
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
    let compositor_directory = Directory::new("td-photo-process");
    let mut compositor = Compositor::start(
        &compositor_directory.0,
        Controls {
            capture: true,
            clipboard: false,
        },
    );
    let client_directory = Directory::new("td-photo-process");
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
    let client = PhotoProcess::start(&client_directory, &compositor.display(), &roll);

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

    // A zoom to 100% over the socket re-develops the box from level 0: the
    // frame is `--preview --develop --zoom` of the roll (the 64x48 frame a
    // photosite a pixel, where the fit showed level 1's 32x24), and back
    // to the fit it is the fitted frame again.
    assert_eq!(
        client.request(8, &["action", "zoom-100"]),
        ["ok", "changed"]
    );
    client.settle(9);
    let state = client.request(10, &["state"]);
    // The zoom is the field before the three export settings and the
    // contrast.
    assert_eq!(
        state.get(state.len().wrapping_sub(5)).map(String::as_str),
        Some("100@5000,5000"),
        "{state:?}"
    );
    let zoomed = super::preview_develop_args(
        &client_directory,
        place.width,
        place.height,
        &roll,
        0,
        &["--zoom"],
    );
    assert_ne!(
        super::box_pixels(&brighter, place.width, r#box),
        super::box_pixels(&zoomed, place.width, r#box),
        "the zoom did not reach the developed pixels"
    );
    assert_eq!(
        compositor.tile(&place),
        zoomed,
        "the developed frame at 100%"
    );
    assert_eq!(
        client.request(11, &["action", "zoom-fit"]),
        ["ok", "changed"]
    );
    client.settle(12);
    assert_eq!(
        compositor.tile(&place),
        brighter,
        "the developed frame fitted again"
    );

    // A contrast edit re-develops as the exposure's did: level 3 again,
    // the frame the roll's preview as the sidecar now is.
    assert_eq!(
        client.request(13, &["action", "contrast-in"]),
        ["ok", "changed"]
    );
    client.settle(14);
    let steeper = super::preview_develop(&client_directory, place.width, place.height, &roll, 0);
    assert_ne!(
        super::box_pixels(&brighter, place.width, r#box),
        super::box_pixels(&steeper, place.width, r#box),
        "the contrast did not reach the developed pixels"
    );
    assert_eq!(
        compositor.tile(&place),
        steeper,
        "the developed frame after the contrast"
    );

    // Auto is measured on the window's pool, off its turn: `wait-idle`
    // holds until the measure has landed and its step is written, the one
    // `edit FILE auto` writes after the same steps, and the frame is the
    // roll's preview as the sidecar now is.
    let apart = client_directory.0.join("apart");
    std::fs::create_dir_all(&apart).unwrap();
    std::fs::copy(roll.join("DSC_0001.NEF"), apart.join("DSC_0001.NEF")).unwrap();
    std::fs::copy(
        roll.join("DSC_0001.NEF.edit"),
        apart.join("DSC_0001.NEF.edit"),
    )
    .unwrap();
    let edited = Command::new(env!("CARGO_BIN_EXE_td-photo"))
        .arg("edit")
        .arg(apart.join("DSC_0001.NEF"))
        .arg("auto")
        .env_clear()
        .output()
        .unwrap();
    assert!(
        edited.status.success(),
        "{}",
        String::from_utf8_lossy(&edited.stderr)
    );
    let expected = std::fs::read_to_string(apart.join("DSC_0001.NEF.edit")).unwrap();
    assert!(expected.contains("\nstep-3 on auto "), "{expected}");
    assert_eq!(client.request(15, &["action", "auto"]), ["ok", "changed"]);
    client.settle(16);
    assert_eq!(
        std::fs::read_to_string(roll.join("DSC_0001.NEF.edit")).unwrap(),
        expected
    );
    let automatic = super::preview_develop(&client_directory, place.width, place.height, &roll, 0);
    assert_ne!(
        super::box_pixels(&steeper, place.width, r#box),
        super::box_pixels(&automatic, place.width, r#box),
        "auto did not reach the developed pixels"
    );
    assert_eq!(
        compositor.tile(&place),
        automatic,
        "the developed frame after auto"
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

    // The picks' auto on the pool: a pick with neither key, written on
    // disk behind the window's back, is measured and given the same step,
    // written by the time `wait-idle` answers.
    let values = expected
        .lines()
        .last()
        .and_then(|line| line.split_once(" auto "))
        .map(|(_, values)| values.to_string())
        .unwrap();
    std::fs::write(
        roll.join("DSC_0001.NEF.edit"),
        "td-photo edit 1\nflag pick\n",
    )
    .unwrap();
    assert_eq!(
        client.request(30, &["action", "auto-picks"]),
        ["ok", "changed"]
    );
    client.settle(31);
    let (exposure, contrast) = values.split_once(' ').unwrap();
    assert_eq!(
        std::fs::read_to_string(roll.join("DSC_0001.NEF.edit")).unwrap(),
        format!(
            "td-photo edit 1\nflag pick\nexposure {exposure}\ncontrast {contrast}\nstep-1 on auto {values}\n"
        )
    );
    // The same pixels as after `auto`, the status row counting the pick.
    assert_eq!(
        super::box_pixels(&compositor.tile(&place), place.width, r#box),
        super::box_pixels(&automatic, place.width, r#box),
        "the developed frame after the picks' auto"
    );
    let text = client.request(32, &["text"]);
    let row = String::from_utf8(td_ui::control::unhex(&text[3]).unwrap()).unwrap();
    assert!(row.contains("| auto on 1 of 1 picks"), "{row}");

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
