#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The widget window against a scripted peer: the toplevel it names, the
//! surface it lays out on configure and keeps through a refused extent,
//! the chords, button phases, wheel travel, focus and close it hands the
//! handler, the poll and wait it runs each turn, the frame it presents
//! from the handler's paint, the clipboard it copies to and pastes from
//! for the handler, and the whole loop over a socket.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;
use td_ui::client::{App, DISPLAY, REGISTRY, SHM, SURFACE, SYNC, TOPLEVEL, XDG_SURFACE};
use td_ui::clipboard::MAX_BYTES;
use td_ui::data::{PLAIN, UTF8};
use td_ui::raster::{Primitive, Raster, Rect, Scale, Surface, PAPER};
use td_ui::wayland::{backing_file, peer, Connection, IDLE_WAIT};
use td_ui::window::{
    run, Clipboard, Flow, Handler, Input, PointerPhase, Refusal, Window, DEFAULT_HEIGHT,
    DEFAULT_WIDTH,
};
use td_ui::wire::{self, Builder, Cursor, Message};

const BLUE: u32 = 0x0033aa;

/// An input as the recorder keeps it: the chord owned.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Record {
    Key(String, bool),
    Pointer(PointerPhase, i64, i64, bool),
    CancelPointer,
    Wheel(isize, isize),
    Resize(usize, usize),
    Focus(bool),
    Close,
    Paste(String),
}

impl Record {
    fn of(input: Input<'_>) -> Self {
        match input {
            Input::Key { chord, repeat } => Self::Key(chord.to_string(), repeat),
            Input::Pointer {
                phase,
                x,
                y,
                extend,
            } => Self::Pointer(phase, x, y, extend),
            Input::CancelPointer => Self::CancelPointer,
            Input::Wheel { rows, columns } => Self::Wheel(rows, columns),
            Input::Resize(surface) => Self::Resize(surface.width, surface.height),
            Input::Focus(focused) => Self::Focus(focused),
            Input::Close => Self::Close,
            Input::Paste(text) => Self::Paste(text.to_string()),
        }
    }
}

/// A handler that records what the window hands it and answers as told.
struct Recorder {
    inputs: Vec<Record>,
    polls: Vec<u64>,
    notices: Vec<String>,
    quit_on: Option<Record>,
    quit_at_poll: Option<u64>,
    wait: u64,
    redraw: bool,
    paints: Vec<Surface>,
    title: String,
    /// A chord (or `press` and `release`, the left button's) on which
    /// the recorder copies the text, and one on which it pastes; what
    /// the clipboard answered each time, and what it said of itself at
    /// each input.
    copy_on: Option<(String, Arc<str>)>,
    paste_on: Option<String>,
    outcomes: Vec<Result<(), Refusal>>,
    states: Vec<(bool, bool, bool)>,
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
            paints: Vec::new(),
            title: "Recorder".into(),
            copy_on: None,
            paste_on: None,
            outcomes: Vec::new(),
            states: Vec::new(),
        }
    }
    fn paints(&self) -> usize {
        self.paints.len()
    }
}

/// One blue fill at the surface's corner over paper.
struct Corner(Surface);

impl td_ui::raster::Composition for Corner {
    fn surface(&self) -> Surface {
        self.0
    }
    fn emit(&self, _: Rect, sink: &mut dyn FnMut(td_ui::raster::Draw)) {
        for (rect, color) in [
            (self.0.bounds(), PAPER),
            (
                Rect {
                    x: 0,
                    y: 0,
                    width: 8,
                    height: 16,
                },
                BLUE,
            ),
        ] {
            sink(td_ui::raster::Draw {
                clip: rect,
                primitive: Primitive::Fill { rect, color },
            });
        }
    }
}

impl Handler for Recorder {
    fn title(&self) -> &str {
        &self.title
    }
    fn app_id(&self) -> &str {
        "td-recorder"
    }
    fn input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) -> Flow {
        let record = Record::of(input);
        self.states.push((
            clipboard.available(),
            clipboard.has_text(),
            clipboard.pasting(),
        ));
        let name = match input {
            Input::Key { chord, .. } => Some(chord.to_string()),
            Input::Pointer {
                phase: PointerPhase::Press,
                ..
            } => Some("press".to_string()),
            Input::Pointer {
                phase: PointerPhase::Release,
                ..
            } => Some("release".to_string()),
            _ => None,
        };
        if let Some(name) = name {
            if let Some((chord, text)) = &self.copy_on {
                if *chord == name {
                    self.outcomes.push(clipboard.copy(text.clone()));
                }
            }
            if self.paste_on.as_deref() == Some(name.as_str()) {
                self.outcomes.push(clipboard.paste());
            }
        }
        let quit = self.quit_on.as_ref() == Some(&record);
        self.inputs.push(record);
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
    fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        self.paints.push(surface);
        self.redraw = false;
        raster
            .paint(&Corner(surface), surface.bounds())
            .map_err(|why| why.to_string())
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

/// A bound window whose compositor also advertises a v3 data-device
/// manager: `(window, peer, keyboard, pointer, device)`, the manager's
/// bind and the device request checked and drained.
fn clipboard_fixture(handler: &mut Recorder) -> (Window<'_, Recorder>, UnixStream, u32, u32, u32) {
    let (a, b) = pair();
    let mut w = Window::new(handler, a, std::env::temp_dir()).unwrap();
    for event in [
        global(1, "wl_compositor", 4),
        global(2, "wl_shm", 1),
        global(3, "xdg_wm_base", 1),
        global(4, "wl_seat", 7),
        global(5, "wl_data_device_manager", 3),
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
    assert!(w.client().clipboard());
    let (requests, _) = drain(&b);
    // The manager's bind names the new id last; the device request on
    // that id names the device first.
    let manager = requests
        .iter()
        .filter(|m| m.object == REGISTRY && m.opcode == 0)
        .find_map(|m| {
            let mut c = Cursor::new(&m.payload);
            c.u32().unwrap();
            let interface = c.string().unwrap();
            c.u32().unwrap();
            let id = c.u32().unwrap();
            (interface == "wl_data_device_manager").then_some(id)
        })
        .unwrap();
    let device = requests
        .iter()
        .find(|m| m.object == manager && m.opcode == 1)
        .map(|m| Cursor::new(&m.payload).u32().unwrap())
        .unwrap();
    (w, b, keyboard, pointer, device)
}

/// A server offer announcing `mimes`, then selected.
fn selection_offer(w: &mut Window<'_, Recorder>, device: u32, id: u32, mimes: &[&str]) {
    w.event(message(device, 0, &[id])).unwrap();
    for mime in mimes {
        w.event(text_request(id, 0, mime)).unwrap();
    }
    w.event(message(device, 5, &[id])).unwrap();
}

/// `wl_data_source.send` for `mime` carrying `file`, across the socket.
fn send_right(
    w: &mut Window<'_, Recorder>,
    peer: &UnixStream,
    source: u32,
    mime: &str,
    file: &File,
) {
    let mut sender = Connection::new(peer.try_clone().unwrap()).unwrap();
    let mut body = Builder::new();
    body.string(mime).unwrap();
    sender.send(source, 1, body, Some(file)).unwrap();
    w.client().connection().read_more().unwrap();
    while let Some(event) = w.client().connection().take().unwrap() {
        w.event(event).unwrap();
    }
}

fn endpoint() -> (UnixStream, File) {
    let (reader, writer) = UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    (reader, File::from(OwnedFd::from(writer)))
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

fn last<'w>(w: &'w Window<'_, Recorder>) -> Option<&'w Record> {
    w.handler().inputs.last()
}

#[test]
fn the_window_names_the_toplevel_and_lays_the_surface_out_on_configure() {
    let mut handler = Recorder::new();
    let (a, peer) = pair();
    let mut w = Window::new(&mut handler, a, std::env::temp_dir()).unwrap();
    assert_eq!(
        (w.surface().width, w.surface().height),
        (DEFAULT_WIDTH, DEFAULT_HEIGHT)
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
    // The binding hands the handler the default surface, so a paint is
    // never its first word of an extent; a configure lays the surface
    // out and is acknowledged; a zero axis keeps the current one.
    let initial = Record::Resize(DEFAULT_WIDTH, DEFAULT_HEIGHT);
    assert_eq!(
        w.handler().inputs.as_slice(),
        std::slice::from_ref(&initial)
    );
    configure(&mut w, 83, 35);
    assert_eq!(w.handler().inputs, [initial, Record::Resize(83, 35)]);
    assert_eq!(w.surface().width, 83);
    assert!(w.client().configured());
    assert_eq!(drain(&peer).0.last(), Some(&message(XDG_SURFACE, 4, &[77])));
    // A configure naming the extent the surface has says nothing to the
    // handler; the compositor sends one for activation.
    let seen = w.handler().inputs.len();
    configure(&mut w, 83, 35);
    assert_eq!(w.handler().inputs.len(), seen);
    configure(&mut w, 0, 64);
    assert_eq!(last(&w), Some(&Record::Resize(83, 64)));
    assert_eq!((w.surface().width, w.surface().height), (83, 64));
    // An extent the raster refuses keeps the last surface, is reported,
    // and delivers no resize for the surface the handler already has.
    let seen = w.handler().inputs.len();
    configure(&mut w, 9000, 64);
    assert_eq!(w.handler().inputs.len(), seen);
    assert_eq!(w.surface().width, 83);
    assert_eq!(w.handler().notices.len(), 1);
    assert!(w.handler().notices[0].starts_with("window extent refused"));
    configure(&mut w, 0, 0);
    assert_eq!(w.surface().width, 83, "the refused extent is forgotten");
    // The close request reaches the handler; the window stays while the
    // handler continues, hears the request again, and closes when the
    // handler quits on it.
    w.event(message(TOPLEVEL, 1, &[])).unwrap();
    assert_eq!(last(&w), Some(&Record::Close));
    assert!(!w.client().closed());
    let seen = w.handler().inputs.len();
    w.event(message(TOPLEVEL, 1, &[])).unwrap();
    assert_eq!(w.handler().inputs.len(), seen + 1);
    assert!(!w.client().closed());
    w.handler_mut().quit_on = Some(Record::Close);
    w.event(message(TOPLEVEL, 1, &[])).unwrap();
    assert!(w.client().closed());
}

#[test]
fn the_window_presents_the_handlers_paint_when_it_is_dirty() {
    let mut handler = Recorder::new();
    let (mut w, peer, _, _) = fixture(&mut handler);
    w.draw().unwrap();
    assert_eq!(w.handler().paints(), 0, "nothing before a configure");
    configure(&mut w, 83, 35);
    drain(&peer);
    w.draw().unwrap();
    assert_eq!(
        w.handler().paints,
        [Surface::new(83, 35, Scale::default()).unwrap()]
    );
    let (requests, files) = drain(&peer);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].metadata().unwrap().len(), 83 * 35 * 4);
    let mut bytes = vec![0; 83 * 35 * 4];
    files[0].read_exact_at(&mut bytes, 0).unwrap();
    assert_eq!(
        pixel(&bytes, 83, 82, 34),
        PAPER,
        "the paper reaches the corner"
    );
    assert_eq!(pixel(&bytes, 83, 3, 3), BLUE, "the corner fill is painted");
    assert_eq!(pixel(&bytes, 83, 8, 0), PAPER);
    assert!(
        requests
            .iter()
            .any(|m| m.object == SURFACE && m.opcode == 6),
        "the frame is committed"
    );
    // Clean after the present: with the frame in flight and nothing
    // changed, the next draws neither paint nor present.
    w.draw().unwrap();
    assert_eq!(w.handler().paints(), 1);
    done(&mut w);
    w.draw().unwrap();
    assert_eq!(w.handler().paints(), 1);
    // The handler asking for a redraw paints and presents again over the
    // same buffer once the compositor released it.
    let buffer = w.client().buffers()[0].id();
    w.event(message(buffer, 0, &[])).unwrap();
    w.handler_mut().redraw = true;
    w.draw().unwrap();
    assert_eq!(w.handler().paints(), 2);
    let (requests, files) = drain(&peer);
    assert!(files.is_empty(), "the buffer is reused");
    assert!(requests
        .iter()
        .any(|m| m.object == SURFACE && m.opcode == 6));
}

#[test]
fn presses_arrive_as_chords_repeat_at_the_turn_and_focus_follows_the_keyboard() {
    let mut handler = Recorder::new();
    let (mut w, peer, keyboard, _) = fixture(&mut handler);
    focus_with_map(&mut w, &peer, keyboard);
    assert_eq!(
        w.handler().inputs,
        [
            Record::Resize(DEFAULT_WIDTH, DEFAULT_HEIGHT),
            Record::Focus(true)
        ]
    );
    w.tick(100).unwrap();
    press(&mut w, keyboard, 9, 30);
    assert_eq!(last(&w), Some(&Record::Key("a".into(), false)));
    // Repeat is armed at the turn's clock; an idle turn before the delay
    // delivers nothing and waits for it, one at the due time delivers the
    // chord again, marked.
    w.end_turn(650, true).unwrap();
    assert_eq!(w.handler().inputs.len(), 3);
    assert_eq!(w.client().connection().wait(), Duration::from_millis(50));
    w.end_turn(700, true).unwrap();
    assert_eq!(w.handler().inputs.len(), 4);
    assert_eq!(last(&w), Some(&Record::Key("a".into(), true)));
    assert_eq!(w.handler().polls, [650, 700]);
    release(&mut w, keyboard, 10, 30);
    // A chord with modifiers is spelled as the keymap spells it; a bare
    // modifier is no press.
    w.event(message(keyboard, 4, &[11, 4, 0, 0, 0])).unwrap();
    press(&mut w, keyboard, 12, 46);
    assert_eq!(last(&w), Some(&Record::Key("C-c".into(), false)));
    release(&mut w, keyboard, 13, 46);
    w.event(message(keyboard, 4, &[14, 1, 0, 0, 0])).unwrap();
    press(&mut w, keyboard, 15, 106);
    assert_eq!(last(&w), Some(&Record::Key("S-Right".into(), false)));
    release(&mut w, keyboard, 16, 106);
    w.event(message(keyboard, 4, &[17, 0, 0, 0, 0])).unwrap();
    let seen = w.handler().inputs.len();
    press(&mut w, keyboard, 18, 29);
    release(&mut w, keyboard, 19, 29);
    assert_eq!(
        w.handler().inputs.len(),
        seen,
        "a bare modifier is no press"
    );
    w.event(message(keyboard, 2, &[20, SURFACE])).unwrap();
    assert_eq!(last(&w), Some(&Record::Focus(false)));
    // The handler quitting on an input closes the window.
    w.event(message(keyboard, 1, &[21, SURFACE, 0])).unwrap();
    w.event(message(keyboard, 4, &[22, 0, 0, 0, 0])).unwrap();
    w.handler_mut().quit_on = Some(Record::Key("q".into(), false));
    press(&mut w, keyboard, 23, 16);
    assert!(w.client().closed());
}

#[test]
fn the_left_button_presses_drags_and_releases_in_surface_pixels_with_shift() {
    let mut handler = Recorder::new();
    let (mut w, peer, keyboard, pointer) = fixture(&mut handler);
    configure(&mut w, 800, 480);
    // A button before entering is nothing; a press after entering lands
    // at the pointer, given in 24.8 fixed point, rounded down.
    w.event(message(pointer, 3, &[1, 0, 0x110, 1])).unwrap();
    assert_eq!(w.handler().inputs.len(), 2);
    w.event(message(
        pointer,
        0,
        &[2, SURFACE, fixed(20) + 128, fixed(35)],
    ))
    .unwrap();
    assert_eq!(w.handler().inputs.len(), 2, "entering is no drag");
    w.event(message(pointer, 3, &[3, 0, 0x110, 1])).unwrap();
    assert_eq!(
        last(&w),
        Some(&Record::Pointer(PointerPhase::Press, 20, 35, false))
    );
    // Motion while held is a drag, past the edge signed; a second press
    // while held is nothing; the release ends it where the pointer is.
    w.event(message(pointer, 2, &[0, fixed(-5), fixed(40)]))
        .unwrap();
    assert_eq!(
        last(&w),
        Some(&Record::Pointer(PointerPhase::Move, -5, 40, false))
    );
    let seen = w.handler().inputs.len();
    w.event(message(pointer, 3, &[4, 0, 0x110, 1])).unwrap();
    assert_eq!(w.handler().inputs.len(), seen);
    w.event(message(pointer, 3, &[5, 0, 0x110, 0])).unwrap();
    assert_eq!(
        last(&w),
        Some(&Record::Pointer(PointerPhase::Release, -5, 40, false))
    );
    // Motion without the button, a release without a press and the right
    // button are nothing.
    let seen = w.handler().inputs.len();
    w.event(message(pointer, 2, &[0, fixed(30), fixed(30)]))
        .unwrap();
    w.event(message(pointer, 3, &[6, 0, 0x110, 0])).unwrap();
    w.event(message(pointer, 3, &[7, 0, 0x111, 1])).unwrap();
    w.event(message(pointer, 3, &[8, 0, 0x111, 0])).unwrap();
    assert_eq!(w.handler().inputs.len(), seen);
    // Leaving while held cancels the drag without a release.
    w.event(message(pointer, 3, &[9, 0, 0x110, 1])).unwrap();
    assert_eq!(
        last(&w),
        Some(&Record::Pointer(PointerPhase::Press, 30, 30, false))
    );
    w.event(message(pointer, 1, &[10, SURFACE])).unwrap();
    assert_eq!(last(&w), Some(&Record::CancelPointer));
    let seen = w.handler().inputs.len();
    w.event(message(pointer, 3, &[11, 0, 0x110, 0])).unwrap();
    assert_eq!(w.handler().inputs.len(), seen, "no release after a cancel");
    // Shift held under a focused, synchronized keyboard extends the press.
    focus_with_map(&mut w, &peer, keyboard);
    w.event(message(keyboard, 4, &[12, 1, 0, 0, 0])).unwrap();
    w.event(message(pointer, 0, &[13, SURFACE, fixed(50), fixed(60)]))
        .unwrap();
    w.event(message(pointer, 3, &[14, 0, 0x110, 1])).unwrap();
    assert_eq!(
        last(&w),
        Some(&Record::Pointer(PointerPhase::Press, 50, 60, true))
    );
    w.event(message(pointer, 3, &[15, 0, 0x110, 0])).unwrap();
    assert_eq!(
        last(&w),
        Some(&Record::Pointer(PointerPhase::Release, 50, 60, false))
    );
    // Losing the pointer capability while held cancels the drag; the
    // keyboard stays, so focus does not move.
    w.event(message(pointer, 3, &[16, 0, 0x110, 1])).unwrap();
    let seat = w.client().seat().unwrap();
    w.event(message(seat, 0, &[2])).unwrap();
    assert_eq!(last(&w), Some(&Record::CancelPointer));
    let seen = w.handler().inputs.len();
    w.event(message(seat, 0, &[3])).unwrap();
    let pointer = w.client().pointer().unwrap();
    w.event(message(pointer, 3, &[17, 0, 0x110, 0])).unwrap();
    assert_eq!(w.handler().inputs.len(), seen, "no release after a cancel");
    // A handler that quits on the cancel a resize makes hears nothing of
    // the resize, nor of anything after.
    w.event(message(pointer, 0, &[18, SURFACE, fixed(50), fixed(60)]))
        .unwrap();
    w.event(message(pointer, 3, &[18, 0, 0x110, 1])).unwrap();
    w.handler_mut().quit_on = Some(Record::CancelPointer);
    let seen = w.handler().inputs.len();
    configure(&mut w, 640, 400);
    assert!(w.client().closed());
    assert_eq!(w.handler().inputs.len(), seen + 1);
    assert_eq!(last(&w), Some(&Record::CancelPointer));
    w.event(message(keyboard, 2, &[19, SURFACE])).unwrap();
    w.event(message(TOPLEVEL, 1, &[])).unwrap();
    assert_eq!(w.handler().inputs.len(), seen + 1, "closed: nothing more");
}

#[test]
fn wheel_frames_arrive_in_cells_while_the_pointer_is_inside() {
    let mut handler = Recorder::new();
    let (mut w, _peer, _, pointer) = fixture(&mut handler);
    configure(&mut w, 800, 480);
    w.event(message(pointer, 0, &[2, SURFACE, fixed(20), fixed(35)]))
        .unwrap();
    // Three rows per discrete step, smooth distance in cells with a
    // remainder carried, nothing for an empty frame.
    w.event(message(pointer, 8, &[0, 1])).unwrap();
    w.event(message(pointer, 4, &[0, 0, fixed(15)])).unwrap();
    w.event(message(pointer, 5, &[])).unwrap();
    assert_eq!(last(&w), Some(&Record::Wheel(3, 0)));
    w.event(message(pointer, 4, &[0, 0, fixed(40)])).unwrap();
    w.event(message(pointer, 5, &[])).unwrap();
    assert_eq!(last(&w), Some(&Record::Wheel(2, 0)));
    w.event(message(pointer, 4, &[0, 1, fixed(-8)])).unwrap();
    w.event(message(pointer, 5, &[])).unwrap();
    assert_eq!(last(&w), Some(&Record::Wheel(0, -1)));
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
    assert_eq!(last(&w), Some(&Record::Focus(true)));
    w.event(message(seat, 0, &[1])).unwrap();
    assert_eq!(last(&w), Some(&Record::Focus(false)));
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
        handler.quit_on = Some(Record::Close);
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
    assert_eq!(handler.paints(), 1);
    assert_eq!(
        handler.inputs,
        [
            Record::Resize(DEFAULT_WIDTH, DEFAULT_HEIGHT),
            Record::Resize(32, 16),
            Record::Close
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
    w.handler_mut().quit_on = Some(Record::Close);
    w.event(message(TOPLEVEL, 1, &[])).unwrap();
    assert!(w.client().closed());
    let seen = w.handler().inputs.len();
    w.end_turn(700, true).unwrap();
    assert_eq!(w.handler().inputs.len(), seen);
    assert_eq!(last(&w), Some(&Record::Close));
}

#[test]
fn a_frame_refused_for_want_of_a_buffer_leaves_the_window_dirty() {
    let mut handler = Recorder::new();
    let (mut w, peer, _, _) = fixture(&mut handler);
    configure(&mut w, 83, 35);
    // Three frames presented and completed but never released hold every
    // buffer busy; the fourth frame finds none, so the handler is not
    // asked to paint and the window stays dirty until one is back.
    for frame in 1..=3 {
        w.handler_mut().redraw = true;
        w.draw().unwrap();
        assert_eq!(w.handler().paints(), frame);
        done(&mut w);
    }
    assert_eq!(drain(&peer).1.len(), 3, "three pools");
    w.handler_mut().redraw = true;
    w.draw().unwrap();
    assert_eq!(w.handler().paints(), 3, "no buffer, no paint");
    assert!(w.client().frame_callback().is_none(), "nothing presented");
    w.handler_mut().redraw = false;
    w.draw().unwrap();
    assert_eq!(w.handler().paints(), 3);
    let buffer = w.client().buffers()[0].id();
    w.event(message(buffer, 0, &[])).unwrap();
    w.draw().unwrap();
    assert_eq!(
        w.handler().paints(),
        4,
        "still dirty, painted once a buffer is back"
    );
    assert!(w.client().frame_callback().is_some(), "presented");
    done(&mut w);
    w.draw().unwrap();
    assert_eq!(w.handler().paints(), 4, "clean again");
}

#[test]
fn the_title_follows_the_handler_after_a_paint() {
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
    // Three frames completed but never released hold every buffer busy:
    // a retitle is then sent on its own, no frame behind it, and the
    // frame that follows once a buffer is back does not send it again.
    done(&mut w);
    drain(&peer);
    w.handler_mut().title = "Recorder (4)".into();
    w.handler_mut().redraw = true;
    w.draw().unwrap();
    let (requests, _) = drain(&peer);
    assert_eq!(
        requests,
        [text_request(TOPLEVEL, 2, "Recorder (4)")],
        "the title alone, no frame"
    );
    let buffer = w.client().buffers()[0].id();
    w.event(message(buffer, 0, &[])).unwrap();
    w.draw().unwrap();
    let (requests, _) = drain(&peer);
    assert!(
        !requests
            .iter()
            .any(|m| m.object == TOPLEVEL && m.opcode == 2),
        "not sent again"
    );
    assert!(requests
        .iter()
        .any(|m| m.object == SURFACE && m.opcode == 6));
}

#[test]
fn a_copy_offers_the_text_at_the_press_and_its_send_crosses_the_right() {
    let mut handler = Recorder::new();
    handler.copy_on = Some(("a".into(), Arc::from("hello, clipboard")));
    let (mut w, peer, keyboard, pointer, device) = clipboard_fixture(&mut handler);
    w.tick(100).unwrap();
    focus_with_map(&mut w, &peer, keyboard);
    drain(&peer);
    press(&mut w, keyboard, 12, 30);
    assert_eq!(w.handler().outcomes, [Ok(())]);
    assert_eq!(w.handler().states.last(), Some(&(true, false, false)));
    // A source with both text MIMEs is the selection at the press's
    // serial.
    let (requests, _) = drain(&peer);
    let source = Cursor::new(&requests[0].payload).u32().unwrap();
    assert_eq!(
        requests,
        [
            message(requests[0].object, 0, &[source]),
            text_request(source, 0, UTF8),
            text_request(source, 0, PLAIN),
            message(device, 1, &[source, 12]),
        ]
    );
    // The send's right receives the text once an idle turn steps the
    // transfer, and the loop waits no longer than the transfer's pace
    // meanwhile; a turn with events still queued steps nothing.
    let (mut reader, writer) = endpoint();
    send_right(&mut w, &peer, source, PLAIN, &writer);
    drop(writer);
    w.end_turn(150, false).unwrap();
    assert_eq!(
        w.client().connection().wait(),
        Duration::from_millis(10),
        "sending: capped"
    );
    w.end_turn(200, true).unwrap();
    let mut text = String::new();
    reader.read_to_string(&mut text).unwrap();
    assert_eq!(text, "hello, clipboard");
    assert_eq!(w.client().connection().wait(), IDLE_WAIT, "sent: no cap");
    assert!(w.handler().notices.is_empty(), "{:?}", w.handler().notices);
    // A repeat has no serial to offer at, and neither has a paste; the
    // left button's press has one.
    release(&mut w, keyboard, 13, 30);
    press(&mut w, keyboard, 14, 30);
    w.end_turn(1000, true).unwrap();
    assert_eq!(last(&w), Some(&Record::Key("a".into(), true)));
    assert_eq!(
        w.handler().outcomes,
        [Ok(()), Ok(()), Err(Refusal::NoSerial)]
    );
    let (requests, _) = drain(&peer);
    let second = Cursor::new(&requests[0].payload).u32().unwrap();
    assert_eq!(
        requests.last(),
        Some(&message(source, 1, &[])),
        "the old source destroyed"
    );
    release(&mut w, keyboard, 15, 30);
    w.handler_mut().copy_on = Some(("press".into(), Arc::from("x".repeat(MAX_BYTES))));
    w.event(message(pointer, 0, &[16, SURFACE, fixed(20), fixed(20)]))
        .unwrap();
    drain(&peer);
    w.event(message(pointer, 3, &[17, 0, 0x110, 1])).unwrap();
    assert_eq!(w.handler().outcomes.last(), Some(&Ok(())));
    let (requests, _) = drain(&peer);
    let big = Cursor::new(&requests[0].payload).u32().unwrap();
    assert!(requests.contains(&message(device, 1, &[big, 17])));
    assert_eq!(requests.last(), Some(&message(second, 1, &[])));
    // A text too long is refused before anything is offered; a copy
    // while the last is still being sent is refused too.
    w.event(message(pointer, 3, &[18, 0, 0x110, 0])).unwrap();
    w.handler_mut().copy_on = Some(("press".into(), Arc::from("x".repeat(MAX_BYTES + 1))));
    w.event(message(pointer, 3, &[19, 0, 0x110, 1])).unwrap();
    assert_eq!(w.handler().outcomes.last(), Some(&Err(Refusal::TooLong)));
    assert!(drain(&peer).0.is_empty());
    w.event(message(pointer, 3, &[20, 0, 0x110, 0])).unwrap();
    // A socket with a read timeout: a send that outlives its cancel
    // fails the read below rather than hanging it.
    let (mut pending, writer) = endpoint();
    send_right(&mut w, &peer, big, UTF8, &writer);
    drop(writer);
    w.end_turn(1300, true).unwrap();
    assert_eq!(
        w.client().connection().wait(),
        Duration::from_millis(10),
        "sending: capped"
    );
    w.handler_mut().copy_on = Some(("press".into(), Arc::from("short")));
    w.event(message(pointer, 3, &[21, 0, 0x110, 1])).unwrap();
    assert_eq!(w.handler().outcomes.last(), Some(&Err(Refusal::Sending)));
    w.event(message(pointer, 3, &[22, 0, 0x110, 0])).unwrap();
    // A second send while one is pending drops exactly its right.
    let (mut other, writer) = endpoint();
    send_right(&mut w, &peer, big, UTF8, &writer);
    drop(writer);
    assert_eq!(other.read(&mut [0]).unwrap(), 0, "dropped");
    // The device released with its manager cancels the send: its
    // endpoint closes short of the text, the wait is no longer capped,
    // a later send of the retired source drops its right, and the next
    // copy finds no device.
    w.event(message(REGISTRY, 1, &[5])).unwrap();
    assert!(!w.client().clipboard());
    let mut prefix = Vec::new();
    pending.read_to_end(&mut prefix).unwrap();
    assert!(prefix.len() < MAX_BYTES, "cancelled short of the text");
    w.end_turn(1400, true).unwrap();
    assert_eq!(w.client().connection().wait(), IDLE_WAIT);
    let (mut other, writer) = endpoint();
    send_right(&mut w, &peer, big, UTF8, &writer);
    drop(writer);
    assert_eq!(other.read(&mut [0]).unwrap(), 0, "retired: dropped");
    assert!(w.handler().notices.is_empty(), "{:?}", w.handler().notices);
    w.event(message(pointer, 3, &[23, 0, 0x110, 1])).unwrap();
    assert_eq!(w.handler().outcomes.last(), Some(&Err(Refusal::NoDevice)));
    assert_eq!(w.handler().states.last(), Some(&(false, false, false)));
}

#[test]
fn the_seat_going_cancels_the_send_and_the_focus_with_it() {
    let mut handler = Recorder::new();
    handler.copy_on = Some(("a".into(), Arc::from("x".repeat(MAX_BYTES))));
    let (mut w, peer, keyboard, _, _) = clipboard_fixture(&mut handler);
    focus_with_map(&mut w, &peer, keyboard);
    drain(&peer);
    press(&mut w, keyboard, 12, 30);
    assert_eq!(w.handler().outcomes, [Ok(())]);
    let (requests, _) = drain(&peer);
    let source = Cursor::new(&requests[0].payload).u32().unwrap();
    let (mut pending, writer) = endpoint();
    send_right(&mut w, &peer, source, UTF8, &writer);
    drop(writer);
    w.end_turn(100, true).unwrap();
    assert_eq!(w.client().connection().wait(), Duration::from_millis(10));
    // The seat's removal releases the data device with it: the window's
    // half cancels the send, so the endpoint closes short of the text,
    // the wait is no longer capped, and the focus is lost.
    w.event(message(REGISTRY, 1, &[4])).unwrap();
    assert!(!w.client().clipboard());
    let mut prefix = Vec::new();
    pending.read_to_end(&mut prefix).unwrap();
    assert!(prefix.len() < MAX_BYTES, "cancelled short of the text");
    assert_eq!(last(&w), Some(&Record::Focus(false)));
    assert!(w.handler().notices.is_empty(), "{:?}", w.handler().notices);
    w.end_turn(200, true).unwrap();
    assert_eq!(w.client().connection().wait(), IDLE_WAIT);
}

#[test]
fn a_release_and_an_unfocused_window_have_no_clipboard_authority() {
    let mut handler = Recorder::new();
    handler.copy_on = Some(("press".into(), Arc::from("unfocused")));
    handler.paste_on = Some("press".into());
    let (mut w, peer, keyboard, pointer, device) = clipboard_fixture(&mut handler);
    selection_offer(&mut w, device, 0xff00_0010, &[UTF8]);
    drain(&peer);
    // The pointer is in but the keyboard is not: neither request has the
    // focus a selection follows.
    w.event(message(pointer, 0, &[16, SURFACE, fixed(20), fixed(20)]))
        .unwrap();
    w.event(message(pointer, 3, &[17, 0, 0x110, 1])).unwrap();
    assert_eq!(
        w.handler().outcomes,
        [Err(Refusal::NoFocus), Err(Refusal::NoFocus)]
    );
    w.event(message(pointer, 3, &[18, 0, 0x110, 0])).unwrap();
    assert!(drain(&peer).0.is_empty());
    // Focused, a copy asked at the button's release has no serial.
    focus_with_map(&mut w, &peer, keyboard);
    drain(&peer);
    w.handler_mut().copy_on = Some(("release".into(), Arc::from("released")));
    w.handler_mut().paste_on = None;
    w.event(message(pointer, 3, &[19, 0, 0x110, 1])).unwrap();
    w.event(message(pointer, 3, &[20, 0, 0x110, 0])).unwrap();
    assert_eq!(
        last(&w),
        Some(&Record::Pointer(PointerPhase::Release, 20, 20, false))
    );
    assert_eq!(w.handler().outcomes.last(), Some(&Err(Refusal::NoSerial)));
    assert!(drain(&peer).0.is_empty());
}

#[test]
fn a_copy_or_a_close_request_drops_the_paste_in_flight_with_a_notice() {
    let mut handler = Recorder::new();
    handler.paste_on = Some("v".into());
    handler.copy_on = Some(("c".into(), Arc::from("replaced")));
    let (mut w, peer, keyboard, _, device) = clipboard_fixture(&mut handler);
    focus_with_map(&mut w, &peer, keyboard);
    selection_offer(&mut w, device, 0xff00_0010, &[UTF8]);
    drain(&peer);
    press(&mut w, keyboard, 9, 47);
    assert_eq!(w.handler().outcomes, [Ok(())]);
    let (_, files) = drain(&peer);
    let mut file = files.into_iter().next().unwrap();
    file.write_all(b"stale").unwrap();
    // The copy replaces the selection the paste was asking: the notice
    // is the copy's, once, and the compositor's selection event that
    // follows finds no paste to cancel.
    release(&mut w, keyboard, 10, 47);
    press(&mut w, keyboard, 11, 46);
    assert_eq!(w.handler().outcomes.last(), Some(&Ok(())));
    assert_eq!(
        w.handler().notices,
        ["paste cancelled: a copy replaced the selection"]
    );
    assert_eq!(w.handler().states.last(), Some(&(true, true, true)));
    let (requests, _) = drain(&peer);
    let source = Cursor::new(&requests[0].payload).u32().unwrap();
    assert!(requests.contains(&message(device, 1, &[source, 11])));
    selection_offer(&mut w, device, 0xff00_0011, &[UTF8]);
    assert_eq!(w.handler().notices.len(), 1);
    drop(file);
    let seen = w.handler().inputs.len();
    w.end_turn(100, true).unwrap();
    assert_eq!(w.handler().inputs.len(), seen);
    assert_eq!(w.client().connection().wait(), IDLE_WAIT);
    // A close request drops the paste too, before the handler hears it.
    release(&mut w, keyboard, 12, 46);
    press(&mut w, keyboard, 13, 47);
    assert_eq!(w.handler().outcomes.last(), Some(&Ok(())));
    let (_, files) = drain(&peer);
    let mut file = files.into_iter().next().unwrap();
    file.write_all(b"late").unwrap();
    w.event(message(TOPLEVEL, 1, &[])).unwrap();
    assert_eq!(last(&w), Some(&Record::Close));
    assert_eq!(w.handler().states.last(), Some(&(true, true, false)));
    assert_eq!(w.handler().notices.len(), 2);
    assert_eq!(w.handler().notices[1], "paste cancelled: close requested");
    drop(file);
    let seen = w.handler().inputs.len();
    w.end_turn(200, true).unwrap();
    assert_eq!(w.handler().inputs.len(), seen);
}

#[test]
fn a_clipboard_request_the_connection_refuses_ends_the_loop() {
    let mut handler = Recorder::new();
    handler.copy_on = Some(("a".into(), Arc::from("unsent")));
    let (mut w, peer, keyboard, _, _) = clipboard_fixture(&mut handler);
    focus_with_map(&mut w, &peer, keyboard);
    drain(&peer);
    // The compositor's end is gone: the source request fails at once,
    // the copy is refused as no device, and the input's turn ends the
    // loop with the write's error rather than the refusal.
    drop(peer);
    let outcome = w.event(message(keyboard, 3, &[12, 0, 30, 1]));
    assert!(
        outcome.as_ref().is_err_and(|e| e.contains("Wayland write")),
        "{outcome:?}"
    );
    assert_eq!(w.handler().outcomes, [Err(Refusal::NoDevice)]);
    assert_eq!(last(&w), Some(&Record::Key("a".into(), false)));
    // The same for a paste's receive request.
    let mut second = Recorder::new();
    second.paste_on = Some("v".into());
    drop(w);
    let (mut w, peer, keyboard, _, device) = clipboard_fixture(&mut second);
    focus_with_map(&mut w, &peer, keyboard);
    selection_offer(&mut w, device, 0xff00_0010, &[UTF8]);
    drain(&peer);
    drop(peer);
    let outcome = w.event(message(keyboard, 3, &[12, 0, 47, 1]));
    assert!(
        outcome.as_ref().is_err_and(|e| e.contains("Wayland write")),
        "{outcome:?}"
    );
    assert_eq!(w.handler().outcomes, [Err(Refusal::NoDevice)]);
}

#[test]
fn a_paste_asks_the_selection_and_its_text_arrives_as_an_input() {
    let mut handler = Recorder::new();
    handler.paste_on = Some("v".into());
    let (mut w, peer, keyboard, _, device) = clipboard_fixture(&mut handler);
    focus_with_map(&mut w, &peer, keyboard);
    // Nothing selected: refused. An offer without text: refused.
    press(&mut w, keyboard, 9, 47);
    assert_eq!(w.handler().outcomes, [Err(Refusal::NoSelection)]);
    selection_offer(&mut w, device, 0xff00_0010, &["image/png"]);
    release(&mut w, keyboard, 10, 47);
    press(&mut w, keyboard, 11, 47);
    assert_eq!(
        w.handler().outcomes.last(),
        Some(&Err(Refusal::NoSelection))
    );
    assert_eq!(w.handler().states.last(), Some(&(true, false, false)));
    // A text offer: the paste asks for it in the selection's preferred
    // spelling, and the text arrives whole at the end of a turn.
    selection_offer(
        &mut w,
        device,
        0xff00_0011,
        &[PLAIN, "text/plain;charset=utf-8"],
    );
    drain(&peer);
    release(&mut w, keyboard, 12, 47);
    press(&mut w, keyboard, 13, 47);
    assert_eq!(w.handler().outcomes.last(), Some(&Ok(())));
    let (requests, files) = drain(&peer);
    assert_eq!(
        requests,
        [text_request(0xff00_0011, 1, "text/plain;charset=utf-8")]
    );
    // A second paste while one is arriving is refused; the loop waits
    // no longer than the transfer's pace.
    release(&mut w, keyboard, 14, 47);
    press(&mut w, keyboard, 15, 47);
    assert_eq!(w.handler().outcomes.last(), Some(&Err(Refusal::Pasting)));
    assert_eq!(w.handler().states.last(), Some(&(true, true, true)));
    w.end_turn(100, false).unwrap();
    assert_eq!(w.client().connection().wait(), Duration::from_millis(10));
    let mut file = files.into_iter().next().unwrap();
    file.write_all("pasted \u{e9}".as_bytes()).unwrap();
    drop(file);
    // A turn with events still queued admits nothing, so a cancellation
    // behind them is seen first; the idle turn delivers the text.
    let seen = w.handler().inputs.len();
    w.end_turn(150, false).unwrap();
    assert_eq!(w.handler().inputs.len(), seen);
    w.end_turn(200, true).unwrap();
    assert_eq!(w.handler().inputs.len(), seen + 1);
    assert_eq!(last(&w), Some(&Record::Paste("pasted \u{e9}".into())));
    assert_eq!(w.handler().states.last(), Some(&(true, true, false)));
    assert_eq!(w.client().connection().wait(), IDLE_WAIT);
    assert!(w.handler().notices.is_empty(), "{:?}", w.handler().notices);
    // A paste cancelled by the selection changing, by focus lost and by
    // the device going is a notice, and never a text.
    release(&mut w, keyboard, 16, 47);
    press(&mut w, keyboard, 17, 47);
    assert_eq!(w.handler().outcomes.last(), Some(&Ok(())));
    let (_, files) = drain(&peer);
    let mut file = files.into_iter().next().unwrap();
    file.write_all(b"stale").unwrap();
    selection_offer(&mut w, device, 0xff00_0012, &[UTF8]);
    assert_eq!(
        w.handler().notices,
        ["paste cancelled: the selection changed"]
    );
    drop(file);
    let seen = w.handler().inputs.len();
    w.end_turn(300, true).unwrap();
    assert_eq!(w.handler().inputs.len(), seen);
    release(&mut w, keyboard, 18, 47);
    press(&mut w, keyboard, 19, 47);
    assert_eq!(w.handler().outcomes.last(), Some(&Ok(())));
    drain(&peer);
    w.event(message(keyboard, 2, &[20, SURFACE])).unwrap();
    assert_eq!(w.handler().notices.len(), 2);
    assert_eq!(w.handler().notices[1], "paste cancelled: focus lost");
    assert_eq!(last(&w), Some(&Record::Focus(false)));
    // The selection followed the focus out: a refocus needs a new offer
    // before a paste can ask.
    w.event(message(keyboard, 1, &[21, SURFACE, 0])).unwrap();
    w.event(message(keyboard, 4, &[22, 0, 0, 0, 0])).unwrap();
    press(&mut w, keyboard, 23, 47);
    assert_eq!(
        w.handler().outcomes.last(),
        Some(&Err(Refusal::NoSelection))
    );
    selection_offer(&mut w, device, 0xff00_0013, &[UTF8]);
    release(&mut w, keyboard, 24, 47);
    press(&mut w, keyboard, 25, 47);
    assert_eq!(w.handler().outcomes.last(), Some(&Ok(())));
    drain(&peer);
    w.event(message(REGISTRY, 1, &[5])).unwrap();
    assert_eq!(w.handler().notices.len(), 3);
    assert_eq!(
        w.handler().notices[2],
        "paste cancelled: the clipboard went away"
    );
    // A paste that stalls past its deadline is a notice too.
    let mut second = Recorder::new();
    second.paste_on = Some("v".into());
    drop(w);
    let (mut w, peer, keyboard, _, device) = clipboard_fixture(&mut second);
    focus_with_map(&mut w, &peer, keyboard);
    selection_offer(&mut w, device, 0xff00_0010, &[UTF8]);
    drain(&peer);
    w.tick(1000).unwrap();
    press(&mut w, keyboard, 9, 47);
    assert_eq!(w.handler().outcomes, [Ok(())]);
    let (_, files) = drain(&peer);
    let _held = files.into_iter().next().unwrap();
    w.end_turn(5999, false).unwrap();
    assert!(w.handler().notices.is_empty());
    w.end_turn(6000, false).unwrap();
    assert_eq!(
        w.handler().notices,
        ["paste failed: clipboard read deadline"]
    );
    assert!(!w
        .handler()
        .inputs
        .iter()
        .any(|r| matches!(r, Record::Paste(_))));
}
