//! The real headless `td-compositor` as a render oracle for the td-setup
//! installer client. td's shared native harness (`td-test-compositor`)
//! launches the compositor from the `TD_TEST_COMPOSITOR` binary, and
//! `SetupProcess` launches the installer as
//! an ordinary client against its Wayland socket. The one case proves the
//! whole live path the wire tests cannot: the client connects, binds, obeys
//! the compositor's configure, accepts a keyboard press, and presents the
//! welcome and unavailable destination pages pixel for pixel.

use super::*;

use td_test_compositor::{
    ppm, Compositor, Controls, Placement, FRAME_BYTES, OUTPUT_HEIGHT, OUTPUT_WIDTH,
};

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
    let compositor_directory = Directory::new("td-setup-process");
    let mut compositor = Compositor::start(
        &compositor_directory.0,
        Controls {
            capture: true,
            clipboard: false,
        },
    );
    let client_directory = Directory::new("td-setup-process");
    let client = SetupProcess::start(&client_directory, &compositor.display());

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
        let request = format!("key {} {time} {code} down", compositor.session());
        let reply = compositor.request(&request, 1024);
        assert_eq!(
            reply,
            format!(
                "ok\ntd-action-v1 session={} action={down_action}\n",
                compositor.session()
            )
            .as_bytes(),
            "{label} key receipt"
        );
        let release = format!("key {} {} {code} up", compositor.session(), time + 1);
        let reply = compositor.request(&release, 1024);
        assert_eq!(
            reply,
            format!(
                "ok\ntd-action-v1 session={} action={up_action}\n",
                compositor.session()
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
    let compositor_directory = Directory::new("td-setup-process");
    let mut compositor = Compositor::start(
        &compositor_directory.0,
        Controls {
            capture: true,
            clipboard: false,
        },
    );
    let client_directory = Directory::new("td-setup-process");
    let client = SetupProcess::start(&client_directory, &compositor.display());
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
        let request = format!("key {} {action} {code} {state}", compositor.session());
        let reply = compositor.request(&request, 1024);
        assert_eq!(
            reply,
            format!(
                "ok\ntd-action-v1 session={} action={action}\n",
                compositor.session()
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

/// F1 on the seat opens the window's key list over welcome: welcome
/// stands around the panel and the panel's title bar is in the selection's
/// colour. Enter, which welcome binds, reaches no page while it is open;
/// Escape closes it, welcome is painted whole again, and Enter then moves
/// on as ever.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn f1_on_the_seat_shows_the_key_list_over_welcome_until_escape() {
    use td_ui::raster::{Scale, Surface, SELECTED};
    let compositor_directory = Directory::new("td-setup-process");
    let mut compositor = Compositor::start(
        &compositor_directory.0,
        Controls {
            capture: true,
            clipboard: false,
        },
    );
    let client_directory = Directory::new("td-setup-process");
    let client = SetupProcess::start(&client_directory, &compositor.display());
    let deadline = Instant::now() + TIMEOUT;
    let place = loop {
        if let Some(place) = compositor.placement("td-setup") {
            break place;
        }
        assert!(Instant::now() < deadline, "unmapped: {}", client.stderr());
        std::thread::sleep(Duration::from_millis(2));
    };
    let welcome = td_setup::preview(place.width, place.height, 1).unwrap();
    assert_frame(&compositor, &place, &welcome, &client, "welcome");
    let mut action = 0;
    let mut press = |compositor: &mut Compositor, code: u32| {
        for state in ["down", "up"] {
            action += 1;
            let request = format!("key {} {action} {code} {state}", compositor.session());
            let reply = compositor.request(&request, 1024);
            assert_eq!(
                reply,
                format!(
                    "ok\ntd-action-v1 session={} action={action}\n",
                    compositor.session()
                )
                .as_bytes(),
                "key {code} {state}"
            );
        }
    };
    let surface = Surface::new(place.width, place.height, Scale::new(1).unwrap()).unwrap();
    let panel = td_ui::keys::Panel::new(surface).unwrap();
    let inside = |rect: td_ui::raster::Rect, x: usize, y: usize| {
        let (x, y) = (x as i64, y as i64);
        x >= rect.x
            && y >= rect.y
            && x < rect.x + i64::from(rect.width)
            && y < rect.y + i64::from(rect.height)
    };
    // The title bar's top row, clear of its text, and its middle column.
    let (probe_x, probe_y) = (
        panel.title.x as usize + panel.title.width as usize / 2,
        panel.title.y as usize,
    );
    let selected = [
        (SELECTED >> 16) as u8,
        (SELECTED >> 8) as u8,
        SELECTED as u8,
    ];
    let at = (probe_y * place.width + probe_x) * 4;
    assert_ne!(
        [welcome[at + 2], welcome[at + 1], welcome[at]],
        selected,
        "welcome alone shows the probe"
    );
    let listed = |x: usize, y: usize| {
        if (x, y) == (probe_x, probe_y) {
            Some(selected)
        } else if inside(panel.frame, x, y) {
            None
        } else {
            let target = (y * place.width + x) * 4;
            Some([welcome[target + 2], welcome[target + 1], welcome[target]])
        }
    };
    press(&mut compositor, 59);
    assert_frame_where(&compositor, &place, &listed, &client, "the key list");
    // Enter would ask for the disks and show the service unavailable.
    press(&mut compositor, 28);
    std::thread::sleep(Duration::from_millis(200));
    assert_frame_where(&compositor, &place, &listed, &client, "the list kept");
    press(&mut compositor, 1);
    assert_frame(&compositor, &place, &welcome, &client, "welcome again");
    press(&mut compositor, 28);
    let unavailable = unavailable_pixels(place.width, place.height);
    assert_frame(&compositor, &place, &unavailable, &client, "unavailable");
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
    let want = |x: usize, y: usize| {
        let target = (y * place.width + x) * 4;
        Some([expected[target + 2], expected[target + 1], expected[target]])
    };
    assert_frame_where(compositor, place, &want, client, label);
}

/// Waits for a settled frame whose every pixel is the RGB `want` gives
/// for its client-relative position, or any where it gives `None`.
fn assert_frame_where(
    compositor: &Compositor,
    place: &Placement,
    want: &dyn Fn(usize, usize) -> Option<[u8; 3]>,
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
        let (output, pixels) = ppm(&capture, compositor.session()).unwrap();
        let second = compositor.observe(&place.window);
        if !second.current || second.commit != first.commit {
            continue;
        }
        assert_eq!(first.client, second.client);
        assert!(output >= first.output && output <= second.output);
        let matches = (0..place.height).all(|y| {
            (0..place.width).all(|x| {
                let source = ((place.y + y) * OUTPUT_WIDTH + place.x + x) * 3;
                want(x, y).is_none_or(|rgb| pixels[source..source + 3] == rgb)
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
