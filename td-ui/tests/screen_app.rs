#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The screen window against a scripted peer: the toplevel it names, the
//! grid it lays out on configure and keeps through a refused extent, the
//! presses, clicks, wheel travel, focus and close it hands the handler,
//! the poll and wait it runs each turn, the frame it presents, and the
//! whole loop over a socket.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::time::Duration;
use td_ui::client::{App, DISPLAY, REGISTRY, SHM, SURFACE, SYNC, TOPLEVEL, XDG_SURFACE};
use td_ui::raster::PAPER;
use td_ui::screen::{Input, Key, Press, Screen, Style, INK};
use td_ui::screen_app::{run, Flow, Window, DEFAULT_HEIGHT, DEFAULT_WIDTH};
use td_ui::wayland::{backing_file, peer, Connection, IDLE_WAIT};
use td_ui::wire::{self, Builder, Cursor, Message};

const BLUE: u32 = 0x0033aa;

/// A handler that records what the window hands it and answers as told.
struct Recorder {
    inputs: Vec<Input>,
    polls: Vec<u64>,
    notices: Vec<String>,
    quit_on: Option<Input>,
    quit_at_poll: Option<u64>,
    wait: u64,
    redraw: bool,
    renders: usize,
    title: String,
}

impl Recorder {
    fn new() -> Self {
        Self {
            inputs: Vec::new(),
            polls: Vec::new(),
            notices: Vec::new(),
            quit_on: None,
            quit_at_poll: None,
            wait: 5000,
            redraw: false,
            renders: 0,
            title: "Recorder".into(),
        }
    }
}

impl td_ui::screen_app::Handler for Recorder {
    fn title(&self) -> &str {
        &self.title
    }
    fn app_id(&self) -> &str {
        "td-recorder"
    }
    fn ground(&self) -> Style {
        Style::new(INK, PAPER)
    }
    fn input(&mut self, input: Input) -> Flow {
        let quit = self.quit_on == Some(input);
        self.inputs.push(input);
        if quit {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }
    fn poll(&mut self, now: u64) -> Flow {
        self.polls.push(now);
        if self.quit_at_poll.is_some_and(|at| now >= at) {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }
    fn wait_ms(&self, _: u64) -> u64 {
        self.wait
    }
    fn needs_redraw(&self) -> bool {
        self.redraw
    }
    fn render(&mut self, screen: &mut Screen) {
        self.renders += 1;
        self.redraw = false;
        screen.clear(self.ground());
        screen.put(0, 0, 'H', Style::new(PAPER, BLUE));
    }
    fn notice(&mut self, message: &str) {
        self.notices.push(message.to_string());
    }
}

fn message(object: u32, opcode: u16, words: &[u32]) -> Message {
    let mut b = Builder::new();
    for w in words {
        b.u32(*w);
    }
    wire::take(&mut b.message(object, opcode).unwrap())
        .unwrap()
        .unwrap()
}

fn global(id: u32, name: &str, version: u32) -> Message {
    let mut b = Builder::new();
    b.u32(id);
    b.string(name).unwrap();
    b.u32(version);
    wire::take(&mut b.message(REGISTRY, 0).unwrap())
        .unwrap()
        .unwrap()
}

fn text_request(object: u32, opcode: u16, text: &str) -> Message {
    let mut body = Builder::new();
    body.string(text).unwrap();
    wire::take(&mut body.message(object, opcode).unwrap())
        .unwrap()
        .unwrap()
}

fn pair() -> (UnixStream, UnixStream) {
    let (a, b) = UnixStream::pair().unwrap();
    b.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
    (a, b)
}

fn drain(peer: &UnixStream) -> (Vec<Message>, Vec<File>) {
    peer::drain(peer).unwrap()
}

/// A bound window whose compositor advertises XRGB and one seat with a
/// keyboard and a pointer: `(window, peer, keyboard, pointer)`, the
/// requests so far drained.
fn fixture(handler: &mut Recorder) -> (Window<'_, Recorder>, UnixStream, u32, u32) {
    let (a, b) = pair();
    let mut w = Window::new(handler, a, std::env::temp_dir()).unwrap();
    for event in [
        global(1, "wl_compositor", 4),
        global(2, "wl_shm", 1),
        global(3, "xdg_wm_base", 1),
        global(4, "wl_seat", 7),
    ] {
        w.event(event).unwrap();
    }
    w.event(message(SYNC, 0, &[0])).unwrap();
    w.event(message(DISPLAY, 1, &[SYNC])).unwrap();
    w.event(message(SHM, 0, &[1])).unwrap();
    let seat = w.client().seat().unwrap();
    w.event(message(seat, 0, &[3])).unwrap();
    let keyboard = w.client().keyboard().unwrap();
    let pointer = w.client().pointer().unwrap();
    drain(&b);
    (w, b, keyboard, pointer)
}

fn configure(w: &mut Window<'_, Recorder>, width: u32, height: u32) {
    w.event(message(TOPLEVEL, 0, &[width, height, 0])).unwrap();
    w.event(message(XDG_SURFACE, 0, &[77])).unwrap();
}

fn map_file() -> File {
    let source = include_str!("fixtures/us.xkb");
    let file = backing_file(&std::env::temp_dir(), source.len() + 1).unwrap();
    file.write_all_at(source.as_bytes(), 0).unwrap();
    file
}

/// Sends the US keymap through the socket, focuses the surface and
/// completes the modifier snapshot so presses translate.
fn focus_with_map(w: &mut Window<'_, Recorder>, peer: &UnixStream, keyboard: u32) {
    let file = map_file();
    let mut sender = Connection::new(peer.try_clone().unwrap()).unwrap();
    let mut body = Builder::new();
    body.u32(1);
    body.u32(file.metadata().unwrap().len() as u32);
    sender.send(keyboard, 0, body, Some(&file)).unwrap();
    w.client().connection().read_more().unwrap();
    while let Some(event) = w.client().connection().take().unwrap() {
        w.event(event).unwrap();
    }
    w.event(message(keyboard, 1, &[3, SURFACE, 0])).unwrap();
    w.event(message(keyboard, 4, &[4, 0, 0, 0, 0])).unwrap();
    w.event(message(keyboard, 5, &[25, 600])).unwrap();
}

fn press(w: &mut Window<'_, Recorder>, keyboard: u32, serial: u32, key: u32) {
    w.event(message(keyboard, 3, &[serial, 0, key, 1])).unwrap();
}

fn release(w: &mut Window<'_, Recorder>, keyboard: u32, serial: u32, key: u32) {
    w.event(message(keyboard, 3, &[serial, 0, key, 0])).unwrap();
}

fn pixel(bytes: &[u8], width: usize, x: usize, y: usize) -> u32 {
    let at = (y * width + x) * 4;
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) & 0xff_ffff
}

fn fixed(pixels: i32) -> u32 {
    (pixels * 256) as u32
}

#[test]
fn the_window_names_the_toplevel_and_lays_the_grid_out_on_configure() {
    let mut handler = Recorder::new();
    let (a, peer) = pair();
    let mut w = Window::new(&mut handler, a, std::env::temp_dir()).unwrap();
    assert_eq!(
        (w.screen().rows(), w.screen().columns()),
        (DEFAULT_HEIGHT / 16, DEFAULT_WIDTH / 8)
    );
    for event in [
        global(1, "wl_compositor", 4),
        global(2, "wl_shm", 1),
        global(3, "xdg_wm_base", 1),
    ] {
        w.event(event).unwrap();
    }
    drain(&peer);
    w.event(message(SYNC, 0, &[0])).unwrap();
    let (requests, _) = drain(&peer);
    let tail = &requests[requests.len() - 3..];
    assert_eq!(
        tail,
        [
            text_request(TOPLEVEL, 2, "Recorder"),
            text_request(TOPLEVEL, 3, "td-recorder"),
            message(SURFACE, 6, &[]),
        ],
        "the title, the app id and a commit close the binding"
    );
    // A configure lays the grid out and is acknowledged; a zero axis keeps
    // the current extent.
    // The binding hands the handler the default grid, so a render is
    // never its first word of an extent.
    let initial = Input::Resize {
        rows: DEFAULT_HEIGHT / 16,
        columns: DEFAULT_WIDTH / 8,
    };
    assert_eq!(w.handler().inputs, [initial]);
    configure(&mut w, 83, 35);
    assert_eq!(
        w.handler().inputs,
        [
            initial,
            Input::Resize {
                rows: 2,
                columns: 10
            }
        ]
    );
    assert_eq!(w.screen().surface().width, 83);
    assert!(w.client().configured());
    assert_eq!(drain(&peer).0.last(), Some(&message(XDG_SURFACE, 4, &[77])));
    // A configure naming the extent the grid has keeps the cells and says
    // nothing to the handler; the compositor sends one for activation.
    w.event(message(SHM, 0, &[1])).unwrap();
    w.draw().unwrap();
    assert_eq!(w.screen().cell(0, 0).unwrap().scalar, 'H');
    let seen = w.handler().inputs.len();
    configure(&mut w, 83, 35);
    assert_eq!(w.handler().inputs.len(), seen);
    assert_eq!(w.screen().cell(0, 0).unwrap().scalar, 'H');
    configure(&mut w, 0, 64);
    assert_eq!(
        w.handler().inputs.last(),
        Some(&Input::Resize {
            rows: 4,
            columns: 10
        })
    );
    assert_eq!(
        (w.screen().surface().width, w.screen().surface().height),
        (83, 64)
    );
    // An extent the raster refuses keeps the last grid, is reported, and
    // delivers no resize for the grid the handler already has.
    let seen = w.handler().inputs.len();
    configure(&mut w, 9000, 64);
    assert_eq!(w.handler().inputs.len(), seen);
    assert_eq!(
        w.handler().inputs.last(),
        Some(&Input::Resize {
            rows: 4,
            columns: 10
        })
    );
    assert_eq!(w.screen().surface().width, 83);
    assert_eq!(w.handler().notices.len(), 1);
    assert!(w.handler().notices[0].starts_with("window extent refused"));
    configure(&mut w, 0, 0);
    assert_eq!(
        w.screen().surface().width,
        83,
        "the refused extent is forgotten"
    );
    // The close request reaches the handler and closes the window.
    w.event(message(TOPLEVEL, 1, &[])).unwrap();
    assert_eq!(w.handler().inputs.last(), Some(&Input::Close));
    assert!(w.client().closed());
}

#[test]
fn the_window_presents_the_rendered_screen_when_it_is_dirty() {
    let mut handler = Recorder::new();
    let (mut w, peer, _, _) = fixture(&mut handler);
    w.draw().unwrap();
    assert_eq!(w.handler().renders, 0, "nothing before a configure");
    configure(&mut w, 83, 35);
    drain(&peer);
    w.draw().unwrap();
    assert_eq!(w.handler().renders, 1);
    let (requests, files) = drain(&peer);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].metadata().unwrap().len(), 83 * 35 * 4);
    let mut bytes = vec![0; 83 * 35 * 4];
    files[0].read_exact_at(&mut bytes, 0).unwrap();
    assert_eq!(pixel(&bytes, 83, 82, 34), PAPER, "the remainder is ground");
    assert_eq!(pixel(&bytes, 83, 8, 0), PAPER, "a blank cell is ground");
    let glyph: Vec<u32> = (0..16).map(|y| pixel(&bytes, 83, 3, y)).collect();
    assert!(glyph.iter().all(|&p| p == BLUE || p == PAPER));
    assert!(
        glyph.contains(&BLUE) && glyph.contains(&PAPER),
        "the H is painted"
    );
    assert!(
        requests
            .iter()
            .any(|m| m.object == SURFACE && m.opcode == 6),
        "the frame is committed"
    );
    // Clean after the present: with the frame in flight and nothing
    // changed, the next draws neither render nor present.
    w.draw().unwrap();
    assert_eq!(w.handler().renders, 1);
    let id = w.client().frame_callback().unwrap();
    w.event(message(id, 0, &[0])).unwrap();
    w.event(message(DISPLAY, 1, &[id])).unwrap();
    w.draw().unwrap();
    assert_eq!(w.handler().renders, 1);
    // The handler asking for a redraw paints and presents again over the
    // same buffer once the compositor released it.
    let buffer = w.client().buffers()[0].id();
    w.event(message(buffer, 0, &[])).unwrap();
    w.handler_mut().redraw = true;
    w.draw().unwrap();
    assert_eq!(w.handler().renders, 2);
    let (requests, files) = drain(&peer);
    assert!(files.is_empty(), "the buffer is reused");
    assert!(requests
        .iter()
        .any(|m| m.object == SURFACE && m.opcode == 6));
}

#[test]
fn presses_translate_repeat_at_the_turn_and_focus_follows_the_keyboard() {
    let mut handler = Recorder::new();
    let (mut w, peer, keyboard, _) = fixture(&mut handler);
    focus_with_map(&mut w, &peer, keyboard);
    assert_eq!(
        w.handler().inputs,
        [
            Input::Resize {
                rows: DEFAULT_HEIGHT / 16,
                columns: DEFAULT_WIDTH / 8
            },
            Input::Focus(true)
        ]
    );
    w.tick(100).unwrap();
    press(&mut w, keyboard, 9, 30);
    assert_eq!(
        w.handler().inputs.last(),
        Some(&Input::Key(Press::plain(Key::Char('a'))))
    );
    // Repeat is armed at the turn's clock; an idle turn at the due time
    // delivers the press again, and the wait follows the repeat.
    w.end_turn(650, true).unwrap();
    assert_eq!(w.handler().inputs.len(), 3);
    assert_eq!(w.client().connection().wait(), Duration::from_millis(50));
    w.end_turn(700, true).unwrap();
    assert_eq!(w.handler().inputs.len(), 4);
    assert_eq!(
        w.handler().inputs.last(),
        Some(&Input::Key(Press::plain(Key::Char('a'))))
    );
    assert_eq!(w.handler().polls, [650, 700]);
    release(&mut w, keyboard, 10, 30);
    // A chord with a modifier; a bare modifier is no press.
    w.event(message(keyboard, 4, &[11, 4, 0, 0, 0])).unwrap();
    press(&mut w, keyboard, 12, 46);
    assert_eq!(
        w.handler().inputs.last(),
        Some(&Input::Key(Press {
            key: Key::Char('c'),
            control: true,
            alt: false,
            shift: false
        }))
    );
    release(&mut w, keyboard, 13, 46);
    let seen = w.handler().inputs.len();
    press(&mut w, keyboard, 14, 29);
    release(&mut w, keyboard, 15, 29);
    assert_eq!(
        w.handler().inputs.len(),
        seen,
        "a bare modifier is no press"
    );
    w.event(message(keyboard, 2, &[16, SURFACE])).unwrap();
    assert_eq!(w.handler().inputs.last(), Some(&Input::Focus(false)));
    // The handler quitting on an input closes the window.
    w.event(message(keyboard, 1, &[17, SURFACE, 0])).unwrap();
    w.event(message(keyboard, 4, &[18, 0, 0, 0, 0])).unwrap();
    w.handler_mut().quit_on = Some(Input::Key(Press::plain(Key::Char('q'))));
    press(&mut w, keyboard, 19, 16);
    assert!(w.client().closed());
}

#[test]
fn clicks_and_wheel_frames_land_on_cells_while_the_pointer_is_inside() {
    let mut handler = Recorder::new();
    let (mut w, _peer, _, pointer) = fixture(&mut handler);
    configure(&mut w, 800, 480);
    // A button before entering is nothing; a click after entering lands
    // in the cell under the pointer, given in 24.8 fixed point.
    w.event(message(pointer, 3, &[1, 0, 0x110, 1])).unwrap();
    assert_eq!(w.handler().inputs.len(), 1);
    w.event(message(pointer, 0, &[2, SURFACE, fixed(20), fixed(35)]))
        .unwrap();
    w.event(message(pointer, 3, &[3, 0, 0x110, 1])).unwrap();
    assert_eq!(
        w.handler().inputs.last(),
        Some(&Input::Click { row: 2, column: 2 })
    );
    w.event(message(pointer, 3, &[4, 0, 0x110, 0])).unwrap();
    w.event(message(pointer, 3, &[5, 0, 0x111, 1])).unwrap();
    assert_eq!(
        w.handler().inputs.len(),
        2,
        "a release and the right button are nothing"
    );
    w.event(message(pointer, 2, &[0, fixed(799) + 128, fixed(479)]))
        .unwrap();
    w.event(message(pointer, 3, &[6, 0, 0x110, 1])).unwrap();
    assert_eq!(
        w.handler().inputs.last(),
        Some(&Input::Click {
            row: 29,
            column: 99
        })
    );
    // Wheel travel: three rows per discrete step, smooth distance in
    // cells with a remainder carried, nothing for an empty frame.
    w.event(message(pointer, 8, &[0, 1])).unwrap();
    w.event(message(pointer, 4, &[0, 0, fixed(15)])).unwrap();
    w.event(message(pointer, 5, &[])).unwrap();
    assert_eq!(
        w.handler().inputs.last(),
        Some(&Input::Wheel {
            rows: 3,
            columns: 0
        })
    );
    w.event(message(pointer, 4, &[0, 0, fixed(40)])).unwrap();
    w.event(message(pointer, 5, &[])).unwrap();
    assert_eq!(
        w.handler().inputs.last(),
        Some(&Input::Wheel {
            rows: 2,
            columns: 0
        })
    );
    w.event(message(pointer, 4, &[0, 1, fixed(-8)])).unwrap();
    w.event(message(pointer, 5, &[])).unwrap();
    assert_eq!(
        w.handler().inputs.last(),
        Some(&Input::Wheel {
            rows: 0,
            columns: -1
        })
    );
    let seen = w.handler().inputs.len();
    w.event(message(pointer, 5, &[])).unwrap();
    assert_eq!(w.handler().inputs.len(), seen);
    // Leaving drops accumulated travel, and axes outside are nothing.
    w.event(message(pointer, 4, &[0, 0, fixed(12)])).unwrap();
    w.event(message(pointer, 1, &[7, SURFACE])).unwrap();
    w.event(message(pointer, 4, &[0, 0, fixed(12)])).unwrap();
    w.event(message(pointer, 5, &[])).unwrap();
    assert_eq!(w.handler().inputs.len(), seen);
}

#[test]
fn every_turn_polls_the_handler_under_its_bounded_wait_and_it_may_quit() {
    let mut handler = Recorder::new();
    let (mut w, peer, _, _) = fixture(&mut handler);
    w.end_turn(10, false).unwrap();
    assert_eq!(w.handler().polls, [10]);
    assert_eq!(
        w.client().connection().wait(),
        IDLE_WAIT,
        "a long wait is capped at the client's idle wait"
    );
    w.handler_mut().wait = 0;
    w.end_turn(20, true).unwrap();
    assert_eq!(w.client().connection().wait(), Duration::from_millis(1));
    w.handler_mut().wait = 40;
    w.end_turn(30, true).unwrap();
    assert_eq!(w.client().connection().wait(), Duration::from_millis(40));
    // Losing the keyboard capability is a focus loss only for a window
    // that had focus: a keyboard that was never there is not one lost.
    let seat = w.client().seat().unwrap();
    let seen = w.handler().inputs.len();
    w.event(message(seat, 0, &[1])).unwrap();
    assert_eq!(w.handler().inputs.len(), seen);
    w.event(message(seat, 0, &[3])).unwrap();
    let keyboard = w.client().keyboard().unwrap();
    focus_with_map(&mut w, &peer, keyboard);
    assert_eq!(w.handler().inputs.last(), Some(&Input::Focus(true)));
    w.event(message(seat, 0, &[1])).unwrap();
    assert_eq!(w.handler().inputs.last(), Some(&Input::Focus(false)));
    let seen = w.handler().inputs.len();
    w.event(message(seat, 0, &[0])).unwrap();
    assert_eq!(w.handler().inputs.len(), seen, "lost once");
    w.handler_mut().quit_at_poll = Some(40);
    w.end_turn(40, true).unwrap();
    assert!(w.client().closed());
    w.end_turn(50, true).unwrap();
    assert_eq!(
        w.handler().polls,
        [10, 20, 30, 40],
        "a closed window stops polling"
    );
}

#[test]
fn the_loop_runs_a_handler_over_a_socket_until_it_quits() {
    let (client, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let worker = std::thread::spawn(move || {
        let mut handler = Recorder::new();
        handler.quit_on = Some(Input::Close);
        let result = run(&mut handler, client, std::env::temp_dir());
        (result, handler)
    });
    let mut handshake = [0; 24];
    peer.read_exact(&mut handshake).unwrap();
    let mut events = Vec::new();
    for event in [
        global(1, "wl_compositor", 4),
        global(2, "wl_shm", 1),
        global(3, "xdg_wm_base", 1),
        message(SYNC, 0, &[0]),
        message(SHM, 0, &[1]),
        message(TOPLEVEL, 0, &[32, 16, 0]),
        message(XDG_SURFACE, 0, &[5]),
    ] {
        let mut body = Builder::new();
        for word in event.payload.as_chunks::<4>().0 {
            body.u32(u32::from_ne_bytes(*word));
        }
        events.extend(body.message(event.object, event.opcode).unwrap());
    }
    peer.write_all(&events).unwrap();
    let mut pending = Vec::new();
    let mut callback = None;
    while callback.is_none() {
        let mut buf = [0; 1024];
        let n = peer.read(&mut buf).unwrap();
        assert_ne!(n, 0);
        pending.extend_from_slice(&buf[..n]);
        while let Some(m) = wire::take(&mut pending).unwrap() {
            if (m.object, m.opcode) == (SURFACE, 3) {
                callback = Some(Cursor::new(&m.payload).u32().unwrap());
            }
        }
    }
    let callback = callback.unwrap();
    let mut events = Vec::new();
    for event in [
        message(callback, 0, &[1]),
        message(DISPLAY, 1, &[callback]),
        message(TOPLEVEL, 1, &[]),
    ] {
        let mut body = Builder::new();
        for word in event.payload.as_chunks::<4>().0 {
            body.u32(u32::from_ne_bytes(*word));
        }
        events.extend(body.message(event.object, event.opcode).unwrap());
    }
    peer.write_all(&events).unwrap();
    let (result, handler) = worker.join().unwrap();
    result.unwrap();
    assert_eq!(handler.renders, 1);
    assert_eq!(
        handler.inputs,
        [
            Input::Resize {
                rows: DEFAULT_HEIGHT / 16,
                columns: DEFAULT_WIDTH / 8
            },
            Input::Resize {
                rows: 1,
                columns: 4
            },
            Input::Close
        ]
    );
    assert!(!handler.polls.is_empty());
}

/// Completes the outstanding frame callback.
fn done(w: &mut Window<'_, Recorder>) {
    let id = w.client().frame_callback().unwrap();
    w.event(message(id, 0, &[0])).unwrap();
    w.event(message(DISPLAY, 1, &[id])).unwrap();
}

#[test]
fn a_repeat_due_after_the_window_closed_is_not_delivered() {
    let mut handler = Recorder::new();
    let (mut w, peer, keyboard, _) = fixture(&mut handler);
    focus_with_map(&mut w, &peer, keyboard);
    w.tick(100).unwrap();
    press(&mut w, keyboard, 9, 30);
    w.event(message(TOPLEVEL, 1, &[])).unwrap();
    assert!(w.client().closed());
    let seen = w.handler().inputs.len();
    w.end_turn(700, true).unwrap();
    assert_eq!(w.handler().inputs.len(), seen);
    assert_eq!(w.handler().inputs.last(), Some(&Input::Close));
}

#[test]
fn a_frame_refused_for_want_of_a_buffer_leaves_the_window_dirty() {
    let mut handler = Recorder::new();
    let (mut w, peer, _, _) = fixture(&mut handler);
    configure(&mut w, 83, 35);
    // Three frames presented and completed but never released hold every
    // buffer busy; the fourth paint finds none, so the next turn paints
    // again without the handler asking, and presents once one is back.
    for frame in 1..=3 {
        w.handler_mut().redraw = true;
        w.draw().unwrap();
        assert_eq!(w.handler().renders, frame);
        done(&mut w);
    }
    assert_eq!(drain(&peer).1.len(), 3, "three pools");
    w.handler_mut().redraw = true;
    w.draw().unwrap();
    assert_eq!(w.handler().renders, 4);
    assert!(w.client().frame_callback().is_none(), "nothing presented");
    w.draw().unwrap();
    assert_eq!(w.handler().renders, 5, "still dirty, painted again");
    let buffer = w.client().buffers()[0].id();
    w.event(message(buffer, 0, &[])).unwrap();
    w.draw().unwrap();
    assert_eq!(w.handler().renders, 6);
    assert!(w.client().frame_callback().is_some(), "presented");
    done(&mut w);
    w.draw().unwrap();
    assert_eq!(w.handler().renders, 6, "clean again");
}

#[test]
fn the_title_follows_the_handler_after_a_render() {
    let mut handler = Recorder::new();
    let (mut w, peer, _, _) = fixture(&mut handler);
    configure(&mut w, 83, 35);
    w.draw().unwrap();
    done(&mut w);
    drain(&peer);
    w.handler_mut().redraw = true;
    w.draw().unwrap();
    assert!(
        !drain(&peer)
            .0
            .iter()
            .any(|m| m.object == TOPLEVEL && m.opcode == 2),
        "an unchanged title is not sent again"
    );
    done(&mut w);
    w.handler_mut().title = "Recorder (3)".into();
    w.handler_mut().redraw = true;
    w.draw().unwrap();
    let (requests, _) = drain(&peer);
    let title = requests
        .iter()
        .position(|m| *m == text_request(TOPLEVEL, 2, "Recorder (3)"))
        .expect("the new title");
    let commit = requests
        .iter()
        .rposition(|m| m.object == SURFACE && m.opcode == 6)
        .expect("the frame's commit");
    assert!(title < commit, "the title goes out with the frame");
}
