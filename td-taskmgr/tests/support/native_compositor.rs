use super::*;

use td_test_compositor::{Compositor, Controls, Placement, OUTPUT_HEIGHT, OUTPUT_WIDTH};

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
            .env("XDG_CONFIG_HOME", directory.0.join("config"))
            .env("TMPDIR", &directory.0)
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

pub(super) fn state(client: &TaskProcess) -> std::collections::BTreeMap<String, String> {
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

/// F12 on the seat is the window's theme chord: it moves the theme,
/// keeps it in the program's file and paints the frame again in it, the
/// state hearing nothing; S-F12 is the state's, and keeps no theme.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn physical_f12_moves_keeps_and_paints_the_theme() {
    let server_directory = Directory::new("td-taskmgr-process");
    let mut compositor = Compositor::start(
        &server_directory.0,
        Controls {
            capture: true,
            clipboard: false,
        },
    );
    let client_directory = Directory::new("td-taskmgr-process");
    let client = TaskProcess::start(&client_directory, &compositor.display());
    let file = client_directory.0.join("config/td-taskmgr/theme");
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
    let before = wait_state(&client, |s| counter(s, "presentations") > 0);
    let chrome = td_ui::theme::HARBOR
        .map(td_ui::raster::CHROME)
        .to_be_bytes();
    compositor.tap(88);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let tile = compositor.tile(&place);
        if std::fs::read_to_string(&file).is_ok_and(|text| text == "harbor\n")
            && tile.as_chunks::<3>().0.iter().any(|p| p[..] == chrome[1..])
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "theme deadline: {}",
            client.stderr()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let after = state(&client);
    assert!(counter(&after, "presentations") > counter(&before, "presentations"));
    // The live counters move with the sampling; where the user is does not.
    for field in ["tab", "focus", "query", "live", "actions", "detail"] {
        assert_eq!(
            after.get(field),
            before.get(field),
            "{field}: F12 is not the state's"
        );
    }
    compositor.key(42, true);
    compositor.tap(88);
    compositor.key(42, false);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "harbor\n");
}

/// F1 on the seat opens the window's key list: its title bar is painted
/// over the frame in the selection's colour; Tab, a key the state binds,
/// reaches nothing while it is open, and a click on a tab closes it and
/// reaches nothing either; opened again, Escape closes it, the frame is
/// the state's again, and Tab then moves the focus.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn physical_f1_shows_the_key_list_until_escape_and_keeps_the_state() {
    use td_ui::raster::{Scale, Surface, SELECTED};
    let server_directory = Directory::new("td-taskmgr-process");
    let mut compositor = Compositor::start(
        &server_directory.0,
        Controls {
            capture: true,
            clipboard: false,
        },
    );
    let client_directory = Directory::new("td-taskmgr-process");
    let client = TaskProcess::start(&client_directory, &compositor.display());
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
    let before = wait_state(&client, |s| counter(s, "presentations") > 0);
    let surface = Surface::new(place.width, place.height, Scale::default()).unwrap();
    let title = td_ui::keys::Panel::new(surface).unwrap().title;
    let selected = &SELECTED.to_be_bytes()[1..];
    // The title bar's top row, above its text, all in the selection's
    // colour: the frame shows the list.
    let listed = |tile: &[u8]| {
        let row = title.y as usize * place.width;
        (title.x as usize..title.x as usize + title.width as usize)
            .all(|x| &tile[(row + x) * 3..(row + x) * 3 + 3] == selected)
    };
    let shown = |compositor: &Compositor, open: bool, label: &str| {
        let deadline = Instant::now() + TIMEOUT;
        while listed(&compositor.tile(&place)) != open {
            assert!(
                Instant::now() < deadline,
                "{label} deadline: {}",
                client.stderr()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    shown(&compositor, false, "the state's frame");
    compositor.tap(59);
    shown(&compositor, true, "the key list");
    compositor.tap(15);
    // The cursor goes to the bottom corner, off the title bar.
    compositor.pointer(place.x + 2, place.y + place.height - 2, 0);
    std::thread::sleep(Duration::from_millis(200));
    assert!(listed(&compositor.tile(&place)), "the list stays open");
    compositor.click(place.x + 350, place.y + 10);
    compositor.pointer(place.x + 2, place.y + place.height - 2, 0);
    shown(&compositor, false, "the click's frame");
    let after = state(&client);
    // The live counters move with the sampling; where the user is does not.
    for field in ["tab", "focus", "query", "live", "actions", "detail"] {
        assert_eq!(
            after.get(field),
            before.get(field),
            "{field}: the list keeps the state's keys and clicks"
        );
    }
    compositor.tap(59);
    shown(&compositor, true, "the key list again");
    compositor.tap(1);
    shown(&compositor, false, "the frame again");
    assert_eq!(state(&client).get("focus"), before.get("focus"));
    compositor.tap(15);
    wait_state(&client, |s| s.get("focus") != before.get("focus"));
    drop(client);
    compositor.stop();
}

/// A click on the toolbar's Help button opens the key list, and a click
/// over the list closes it, reaching nothing behind it; a reading key
/// held through that click stops repeating with it, so the state behind
/// hears none of its repeats.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn physical_help_click_shows_the_key_list_and_a_click_closes_it() {
    use td_ui::raster::{Scale, Surface, SELECTED};
    let server_directory = Directory::new("td-taskmgr-process");
    let mut compositor = Compositor::start(
        &server_directory.0,
        Controls {
            capture: true,
            clipboard: false,
        },
    );
    let client_directory = Directory::new("td-taskmgr-process");
    let client = TaskProcess::start(&client_directory, &compositor.display());
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
    wait_state(&client, |s| counter(s, "presentations") > 0);
    // The tree's first row, from which a repeated Down would move.
    compositor.tap(102);
    let before = wait_state(&client, |s| s.get("selected").is_some_and(|s| s != "none"));
    let surface = Surface::new(place.width, place.height, Scale::default()).unwrap();
    // The whole Help button is on the tile.
    let help = td_taskmgr::ui::toolbar(surface, 4);
    assert!(help.width > 0 && help.x as usize + help.width as usize <= place.width);
    let title = td_ui::keys::Panel::new(surface).unwrap().title;
    let selected = &SELECTED.to_be_bytes()[1..];
    let listed = |tile: &[u8]| {
        let row = title.y as usize * place.width;
        (title.x as usize..title.x as usize + title.width as usize)
            .all(|x| &tile[(row + x) * 3..(row + x) * 3 + 3] == selected)
    };
    let shown = |compositor: &Compositor, open: bool, label: &str| {
        let deadline = Instant::now() + TIMEOUT;
        while listed(&compositor.tile(&place)) != open {
            assert!(
                Instant::now() < deadline,
                "{label} deadline: {}",
                client.stderr()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    shown(&compositor, false, "the state's frame");
    compositor.click(place.x + help.x as usize + 8, place.y + help.y as usize + 8);
    shown(&compositor, true, "the key list");
    // Down, held past the 600 ms delay, repeats while it scrolls the list.
    compositor.key(108, true);
    std::thread::sleep(Duration::from_millis(900));
    compositor.click(place.x + 2, place.y + place.height - 2);
    shown(&compositor, false, "the frame again");
    // At 25 a second, a repeat left armed would move the selection.
    std::thread::sleep(Duration::from_millis(600));
    let after = state(&client);
    compositor.key(108, false);
    for field in [
        "tab", "focus", "query", "live", "actions", "detail", "selected",
    ] {
        assert_eq!(
            after.get(field),
            before.get(field),
            "{field}: the clicks and the held key reached nothing behind the list"
        );
    }
    // Down reaches the state now, so its repeats would have shown.
    compositor.tap(108);
    wait_state(&client, |s| s.get("selected") != before.get("selected"));
    drop(client);
    compositor.stop();
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn live_history_graph_tree_and_hidden_window_collection() {
    let server_directory = Directory::new("td-taskmgr-process");
    let mut compositor = Compositor::start(
        &server_directory.0,
        Controls {
            capture: true,
            clipboard: false,
        },
    );
    let client_directory = Directory::new("td-taskmgr-process");
    let client = TaskProcess::start(&client_directory, &compositor.display());
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
    compositor.tap(15);
    wait_state(&client, |s| s.get("focus").is_some_and(|s| s == "Graph"));
    compositor.tap(102);
    compositor.tap(57); // Clear the series: ranked contributors at this time.
    let inspected = wait_state(&client, |s| s.get("live").is_some_and(|s| s == "false"));
    let pinned = counter(&inspected, "inspected_ns");
    compositor.tap(28);
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
    compositor.tap(3);
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
    compositor.tap(2);
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

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn physical_submenus_confirm_owned_stop_and_resume() {
    let directory = Directory::new("td-taskmgr-process");
    let compositor = std::cell::RefCell::new(Compositor::start(
        &directory.0,
        Controls {
            capture: true,
            clipboard: false,
        },
    ));
    let client_directory = Directory::new("td-taskmgr-process");
    let client = TaskProcess::start(&client_directory, &compositor.borrow().display());
    let deadline = Instant::now() + TIMEOUT;
    let place = loop {
        if let Some(place) = compositor.borrow().placement("td-taskmgr") {
            break place;
        }
        assert!(Instant::now() < deadline, "mapping deadline");
        std::thread::sleep(Duration::from_millis(20));
    };
    wait_state(&client, |s| counter(s, "retained") > 0);
    exercise_controls(
        &client,
        |name| {
            let code = match name {
                "F10" => 68,
                "Right" => 106,
                "Down" => 108,
                "Return" => 28,
                "Tab" => 15,
                "Escape" => 1,
                _ => panic!("unknown physical fixture key"),
            };
            compositor.borrow_mut().tap(code);
        },
        || {
            // The client reports its confirmation before its frame with the
            // confirmation is committed, so a settled frame captured at once
            // can still be the menu's: capture until the band matches, and
            // compare the last capture at the deadline.
            let (rows, expected) = expected_confirmation_cancel(&place);
            let deadline = Instant::now() + TIMEOUT;
            loop {
                let pixels = compositor.borrow().tile(&place);
                let shown = rows
                    .iter()
                    .all(|&(begin, end)| pixels.get(begin..end) == expected.get(begin..end));
                if shown {
                    break;
                }
                if Instant::now() >= deadline {
                    for &(begin, end) in &rows {
                        assert_eq!(
                            &pixels[begin..end],
                            &expected[begin..end],
                            "mapped default-Cancel band"
                        );
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        },
    );
    assert_eq!(client.request(3, &["action", "quit"]), ["ok", "quit"]);
    assert!(client.finish());
    compositor.borrow_mut().stop();
}

// The default-Cancel band's byte ranges in a tile's RGB rows, and the RGB
// rows of an independently constructed shared confirmation they must equal;
// unrelated search/menu changes cannot match them.
fn expected_confirmation_cancel(place: &Placement) -> (Vec<(usize, usize)>, Vec<u8>) {
    use td_ui::confirmations::{Controller, Focus, Model};
    use td_ui::raster::{self, Raster, Rect, Surface};
    let surface = Surface::new(place.width, place.height, Default::default()).unwrap();
    let rect = Rect {
        x: 16,
        y: 24,
        width: place.width as u32 - 32,
        height: place.height as u32 - 72,
    };
    let model = Model::new(
        "Expected confirmation",
        "Send signal",
        &["owned process"],
        (),
        1u64,
    )
    .unwrap();
    let widget = Controller::new(model, surface, rect, Some(())).unwrap();
    let band = widget.action_rect(Focus::Cancel).unwrap();
    let mut expected = vec![0; place.width * place.height * 4];
    let font = td_ui::font::pinned().unwrap();
    let mut raster = Raster::new(&mut expected, &font, surface, place.width * 4).unwrap();
    widget.emit(surface.bounds(), &mut |draw| raster.draw(draw));
    let expected = raster::rgb(&expected, surface, place.width * 4).unwrap();
    let rows = (band.y as usize..band.y as usize + band.height as usize)
        .map(|y| {
            let begin = (y * place.width + band.x as usize) * 3;
            (begin, begin + band.width as usize * 3)
        })
        .collect();
    (rows, expected)
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn physical_double_click_opens_history_and_back_restores_the_ranked_list() {
    use td_ui::{
        raster::{Rect, Surface},
        split,
    };
    let directory = Directory::new("td-taskmgr-process");
    let mut compositor = Compositor::start(
        &directory.0,
        Controls {
            capture: true,
            clipboard: false,
        },
    );
    let client_directory = Directory::new("td-taskmgr-process");
    let client = TaskProcess::start(&client_directory, &compositor.display());
    let deadline = Instant::now() + TIMEOUT;
    let place = loop {
        if let Some(place) = compositor.placement("td-taskmgr") {
            break place;
        }
        assert!(Instant::now() < deadline, "mapping deadline");
        std::thread::sleep(Duration::from_millis(20));
    };
    wait_state(&client, |s| counter(s, "retained") >= 2);
    compositor.click(place.x + 200, place.y + 10);
    compositor.tap(15); // Graph focus.
    compositor.tap(107); // Pin the newest observation, avoiding process churn during gestures.
    wait_state(&client, |s| s.get("live").is_some_and(|s| s == "false"));
    // Target the actual client extent through the shared split geometry.
    let surface = Surface::new(place.width, place.height, Default::default()).unwrap();
    let layout = split::Controller::new(
        split::Config {
            axis: split::Axis::Vertical,
            first_min: 216,
            second_min: 96,
        },
        split::Share::default(),
        surface,
        Rect {
            x: 0,
            y: 48,
            width: place.width as u32,
            height: place.height as u32 - 72,
        },
    )
    .unwrap()
    .layout()
    .unwrap();
    compositor.click(place.x + 550, place.y + layout.second.y as usize + 28);
    compositor.tap(102); // Highest CPU row after the global sort.
    let selected = wait_state(&client, |s| {
        s.get("selected")
            .is_some_and(|s| s != "none" && s != "group")
    });
    let key = selected.get("selected").unwrap().clone();
    let before = compositor.tile(&place);
    let x = place.x + 120;
    let y = place.y + layout.second.y as usize + 52;
    compositor.click(x, y);
    compositor.click(x, y);
    wait_state(&client, |s| s.get("detail") == Some(&key));
    // The detail is reported before the frame showing it is committed, and a
    // still status line leaves a stale capture equal to `before`.
    let deadline = Instant::now() + TIMEOUT;
    while compositor.tile(&place) == before {
        assert!(Instant::now() < deadline, "history detail never drawn");
        std::thread::sleep(Duration::from_millis(20));
    }
    let text = client.request(4, &["text"]);
    let text = String::from_utf8(td_ui::control::unhex(text.last().unwrap()).unwrap()).unwrap();
    assert!(text.contains("CPU time:"), "{text}");
    assert!(text.contains("Back (Escape)"), "{text}");
    compositor.tap(1);
    let restored = wait_state(&client, |s| s.get("detail").is_some_and(|s| s == "none"));
    assert_eq!(restored.get("selected"), Some(&key));
    assert_eq!(restored.get("rows"), selected.get("rows"));
    let _ = compositor.tile(&place);
    assert_eq!(client.request(3, &["action", "quit"]), ["ok", "quit"]);
    assert!(client.finish());
    compositor.stop();
}
