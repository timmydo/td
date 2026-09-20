#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The shared client against a scripted peer: the object table and
//! registry, the toplevel's buffers and frame callback, the seat with its
//! keyboard and pointer, the pointer image, the clipboard's device,
//! offers and sources, and the turn loop, driven by the smallest
//! consumer. The presentation, loop, device and clipboard lifecycle
//! tests moved here from td-editor's window tests.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use td_ui::client::{
    run, App, Client, ClipboardEvent, Handled, KeyboardEvent, Kind, Tag, BUFFERS, COMPOSITOR,
    DISPLAY, GLOBALS, INITIAL_DEADLINE, MESSAGES_PER_TURN, NAME_BYTES, OBJECTS, REGISTRY, SHM,
    SURFACE, SYNC, TOPLEVEL, WM, XDG_SURFACE,
};
use td_ui::data::{ANNOUNCEMENTS, OFFER_LIMIT, PLAIN, UTF8};
use td_ui::keyboard::Held;
use td_ui::pointer;
use td_ui::wayland::{backing_file, cursor_pixels, peer, Connection, IDLE_WAIT, WRITE_DEADLINE};
use td_ui::wire::{self, Builder, Cursor, Message};

type Result<T> = std::result::Result<T, String>;

/// A consumer's objects: one live, one waiting for `delete_id`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mark {
    Live,
    Gone,
}

impl Tag for Mark {
    fn retired(self) -> bool {
        self == Mark::Gone
    }
}

/// The smallest consumer: one flat colour behind a marker pixel, a log of
/// what the client hands back, and optionally one object of its own
/// whose opcode 0 carries a right.
struct Probe {
    client: Client<Mark>,
    clock: u64,
    keyboard: Vec<KeyboardEvent>,
    pointer: Vec<pointer::Event>,
    capabilities: Vec<(bool, bool)>,
    seat_removed: usize,
    clipboard: Vec<&'static str>,
    size: (usize, usize),
    dirty: bool,
    frames: usize,
    unhandled: usize,
    wants_right: bool,
    parked: Option<u32>,
    waits: usize,
    rights: usize,
    shortened: bool,
    log: Vec<(u32, u16)>,
}

const FILL: u8 = 0xcf;
const MARK: [u8; 4] = [0x3f, 0x45, 0x48, 0xff];

impl Probe {
    fn new(stream: UnixStream) -> Self {
        Self {
            client: Client::new(stream, std::env::temp_dir()).unwrap(),
            clock: 0,
            keyboard: Vec::new(),
            pointer: Vec::new(),
            capabilities: Vec::new(),
            seat_removed: 0,
            clipboard: Vec::new(),
            size: (0, 0),
            dirty: false,
            frames: 0,
            unhandled: 0,
            wants_right: false,
            parked: None,
            waits: 0,
            rights: 0,
            shortened: false,
            log: Vec::new(),
        }
    }
}

impl App for Probe {
    type Tag = Mark;

    fn client(&mut self) -> &mut Client<Mark> {
        &mut self.client
    }

    fn needs_descriptor(&self, message: &Message) -> Result<bool> {
        Ok(self.parked == Some(message.object) && message.opcode == 0)
    }

    fn descriptor_wait(&mut self) {
        self.waits += 1;
    }

    fn tick(&mut self, _: u64) -> Result<()> {
        Ok(())
    }

    fn event(&mut self, message: Message) -> Result<()> {
        self.log.push((message.object, message.opcode));
        match self.client.handle(&message, self.clock)? {
            Handled::Bound => {
                self.client.set_title("probe")?;
                self.client.set_app_id("td-ui-probe")?;
                self.client.commit()?;
                if self.wants_right {
                    self.parked = Some(self.client.allocate(Mark::Live)?);
                }
                Ok(())
            }
            Handled::Configure { size, serial } => {
                if let Some((width, height)) = size {
                    let (current_width, current_height) = self.size;
                    self.size = (
                        if width == 0 {
                            current_width
                        } else {
                            width as usize
                        },
                        if height == 0 {
                            current_height
                        } else {
                            height as usize
                        },
                    );
                }
                self.client.acknowledge(serial)?;
                self.dirty = true;
                Ok(())
            }
            Handled::CloseRequested => {
                self.client.close();
                Ok(())
            }
            Handled::FrameDone => {
                self.frames += 1;
                Ok(())
            }
            Handled::GlobalRemoved { required: true, .. } => {
                Err("required Wayland global was removed".into())
            }
            Handled::Unhandled => {
                if self.needs_descriptor(&message)? {
                    self.client.pop_descriptor().ok_or("missing right")?;
                    self.rights += 1;
                }
                self.unhandled += 1;
                Ok(())
            }
            Handled::Keyboard(event) => {
                self.keyboard.push(event);
                Ok(())
            }
            Handled::Pointer(event) => {
                self.pointer.push(event);
                Ok(())
            }
            Handled::Capabilities { keyboard, pointer } => {
                self.capabilities.push((keyboard, pointer));
                Ok(())
            }
            Handled::SeatRemoved => {
                self.seat_removed += 1;
                Ok(())
            }
            Handled::Clipboard(event) => {
                self.clipboard.push(match event {
                    ClipboardEvent::Selection => "selection",
                    ClipboardEvent::Send(right) => {
                        File::from(right)
                            .write_all(b"probe text")
                            .map_err(|e| e.to_string())?;
                        "send"
                    }
                    ClipboardEvent::Cancelled => "cancelled",
                    ClipboardEvent::Released => "released",
                });
                Ok(())
            }
            Handled::Done | Handled::GlobalRemoved { .. } => Ok(()),
        }
    }

    fn end_turn(&mut self, _: u64, _: bool) -> Result<()> {
        // Shorten one turn's wait; every later turn must start over.
        if self.wants_right && !self.shortened {
            self.client.connection().set_wait(Duration::from_millis(1));
            self.shortened = true;
        }
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let (width, height) = self.size;
        let presented = self.client.present(width, height, &mut |pixels| {
            pixels.fill(FILL);
            pixels
                .get_mut(..4)
                .ok_or("marker pixel")?
                .copy_from_slice(&MARK);
            Ok(())
        })?;
        if presented {
            self.dirty = false;
        }
        Ok(())
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

fn pair() -> (UnixStream, UnixStream) {
    let (a, b) = UnixStream::pair().unwrap();
    b.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
    (a, b)
}

/// A bound probe whose compositor advertises XRGB.
fn fixture() -> (Probe, UnixStream) {
    let (a, b) = pair();
    let mut probe = Probe::new(a);
    for event in [
        global(30, "wl_compositor", 6),
        global(20, "wl_shm", 1),
        global(90, "xdg_wm_base", 5),
        global(70, "ignored_optional", 42),
    ] {
        probe.event(event).unwrap();
    }
    probe.event(message(SYNC, 0, &[123])).unwrap();
    probe.event(message(DISPLAY, 1, &[SYNC])).unwrap();
    probe.event(message(SHM, 0, &[1])).unwrap();
    (probe, b)
}

fn configure(p: &mut Probe, width: u32, height: u32) {
    p.event(message(TOPLEVEL, 0, &[width, height, 0])).unwrap();
    p.event(message(XDG_SURFACE, 0, &[77])).unwrap();
}

fn drain(peer: &UnixStream) -> (Vec<Message>, Vec<File>) {
    peer::drain(peer).unwrap()
}

fn done(p: &mut Probe) {
    let id = p.client.frame_callback().unwrap();
    p.event(message(id, 0, &[0])).unwrap();
    p.event(message(DISPLAY, 1, &[id])).unwrap();
}

fn bind_target(request: &Message) -> (String, u32, u32) {
    assert_eq!((request.object, request.opcode), (REGISTRY, 0));
    let mut c = Cursor::new(&request.payload);
    c.u32().unwrap();
    let name = c.string().unwrap();
    let version = c.u32().unwrap();
    let id = c.u32().unwrap();
    c.finish().unwrap();
    (name, version, id)
}

fn text_event(object: u32, opcode: u16, text: &str) -> Message {
    let mut body = Builder::new();
    body.string(text).unwrap();
    wire::take(&mut body.message(object, opcode).unwrap())
        .unwrap()
        .unwrap()
}

/// A bound probe whose compositor advertises XRGB and one v10 seat, with
/// its pointer and keyboard already created: `(probe, peer, seat,
/// keyboard, pointer)`, the requests so far drained.
fn seat_fixture() -> (Probe, UnixStream, u32, u32, u32) {
    let (a, b) = pair();
    let mut p = Probe::new(a);
    for event in [
        global(1, "wl_compositor", 4),
        global(2, "wl_shm", 1),
        global(3, "xdg_wm_base", 1),
        global(4, "wl_seat", 10),
    ] {
        p.event(event).unwrap();
    }
    p.event(message(SYNC, 0, &[0])).unwrap();
    p.event(message(DISPLAY, 1, &[SYNC])).unwrap();
    p.event(message(SHM, 0, &[1])).unwrap();
    let seat = p.client.seat().unwrap();
    let (requests, _) = drain(&b);
    let binding = requests.iter().rfind(|m| m.object == REGISTRY).unwrap();
    assert_eq!(
        bind_target(binding),
        ("wl_seat".into(), 7, seat),
        "a v10 seat binds at the v7 cap"
    );
    p.event(message(seat, 0, &[3])).unwrap();
    let keyboard = p.client.keyboard().unwrap();
    let pointer = p.client.pointer().unwrap();
    drain(&b);
    (p, b, seat, keyboard, pointer)
}

/// The libxkbcommon US map in an unlinked file with its trailing NUL.
fn map_file() -> File {
    let source = include_str!("fixtures/us.xkb");
    let file = backing_file(&std::env::temp_dir(), source.len() + 1).unwrap();
    file.write_all_at(source.as_bytes(), 0).unwrap();
    file
}

/// Sends `file` as `keyboard`'s keymap of `format` through the socket and
/// dispatches everything the probe reads back.
fn send_map(p: &mut Probe, peer: &UnixStream, keyboard: u32, format: u32, file: &File) {
    let mut sender = Connection::new(peer.try_clone().unwrap()).unwrap();
    let mut body = Builder::new();
    body.u32(format);
    body.u32(file.metadata().unwrap().len() as u32);
    sender.send(keyboard, 0, body, Some(file)).unwrap();
    p.client.connection().read_more().unwrap();
    while let Some(event) = p.client.connection().take().unwrap() {
        p.event(event).unwrap();
    }
}

fn press(p: &mut Probe, keyboard: u32, serial: u32, key: u32) {
    p.event(message(keyboard, 3, &[serial, 0, key, 1])).unwrap();
}

fn release(p: &mut Probe, keyboard: u32, serial: u32, key: u32) {
    p.event(message(keyboard, 3, &[serial, 0, key, 0])).unwrap();
}

#[test]
fn the_budgets_and_fixed_ids_are_the_editors() {
    assert_eq!(
        (
            DISPLAY,
            REGISTRY,
            SYNC,
            COMPOSITOR,
            SHM,
            WM,
            SURFACE,
            XDG_SURFACE,
            TOPLEVEL
        ),
        (1, 2, 3, 4, 5, 6, 7, 8, 9)
    );
    assert_eq!(
        (OBJECTS, GLOBALS, NAME_BYTES, BUFFERS, MESSAGES_PER_TURN),
        (128, 128, 256, 3, 256)
    );
    assert_eq!(INITIAL_DEADLINE, Duration::from_secs(20));
}

#[test]
fn fixed_ids_are_published_in_order_and_pools_cross_the_socket_pixel_exact() {
    let (mut p, peer) = fixture();
    p.draw().unwrap();
    assert!(p.client.buffers().is_empty(), "no buffer before configure");
    configure(&mut p, 800, 600);
    p.draw().unwrap();
    let (messages, mut files) = drain(&peer);
    assert_eq!(files.len(), 1);
    let mut file = files.remove(0);
    let metadata = file.metadata().unwrap();
    assert_eq!(metadata.nlink(), 0);
    assert_eq!(metadata.mode() & 0o777, 0o600);
    let mut pixels = Vec::new();
    file.read_to_end(&mut pixels).unwrap();
    assert_eq!(pixels.len(), 800 * 600 * 4);
    assert_eq!(pixels, p.client.pixels());
    assert_eq!(pixels[..4], MARK);
    assert!(pixels[4..].iter().all(|byte| *byte == FILL));
    for (request, expected) in messages.iter().zip([
        ("wl_compositor", 4, COMPOSITOR),
        ("wl_shm", 1, SHM),
        ("xdg_wm_base", 1, WM),
    ]) {
        let (name, version, id) = bind_target(request);
        assert_eq!((name.as_str(), version, id), expected);
    }
    assert_eq!(messages[3], message(COMPOSITOR, 0, &[SURFACE]));
    assert_eq!(messages[4], message(WM, 2, &[XDG_SURFACE, SURFACE]));
    assert_eq!(messages[5], message(XDG_SURFACE, 1, &[TOPLEVEL]));
    for (request, (opcode, text)) in messages[6..8]
        .iter()
        .zip([(2, "probe"), (3, "td-ui-probe")])
    {
        assert_eq!((request.object, request.opcode), (TOPLEVEL, opcode));
        let mut c = Cursor::new(&request.payload);
        assert_eq!(c.string().unwrap(), text);
        c.finish().unwrap();
    }
    assert_eq!(messages[8], message(SURFACE, 6, &[]));
    assert_eq!(messages[9], message(XDG_SURFACE, 4, &[77]));
    assert_eq!(messages.last().unwrap(), &message(SURFACE, 6, &[]));
    assert!(messages.contains(&message(XDG_SURFACE, 3, &[0, 0, 800, 600])));
    assert!(messages.contains(&message(SURFACE, 9, &[0, 0, 800, 600])));
}

#[test]
fn callback_is_not_release_and_three_busy_buffers_bound_resize_storms() {
    let (mut p, peer) = fixture();
    configure(&mut p, 400, 200);
    p.draw().unwrap();
    let first = p.client.buffers()[0].id();
    let (_, files) = drain(&peer);
    let mut original = vec![0; 400 * 200 * 4];
    files[0].read_exact_at(&mut original, 0).unwrap();
    configure(&mut p, 500, 200);
    p.draw().unwrap();
    assert_eq!(p.client.buffers().len(), 1, "callback throttles");
    done(&mut p);
    assert_eq!(p.frames, 1);
    p.draw().unwrap();
    assert_eq!(p.client.buffers().len(), 2);
    drain(&peer);
    done(&mut p);
    configure(&mut p, 600, 200);
    p.draw().unwrap();
    assert_eq!(p.client.buffers().len(), BUFFERS);
    drain(&peer);
    done(&mut p);
    configure(&mut p, 700, 200);
    configure(&mut p, 0, 240);
    p.draw().unwrap();
    assert!(p.dirty);
    assert!(p.client.frame_callback().is_none());
    assert_eq!(p.client.buffers().len(), BUFFERS);
    assert_eq!(p.size, (700, 240));
    let mut still_original = vec![0; original.len()];
    files[0].read_exact_at(&mut still_original, 0).unwrap();
    assert_eq!(still_original, original);
    p.event(message(first, 0, &[])).unwrap();
    p.draw().unwrap();
    assert!(!p.dirty);
    assert_eq!(p.client.buffers().len(), BUFFERS);
    assert_eq!(p.client.buffers().last().unwrap().size(), (700, 240));
    assert_eq!(p.client.kind(first).unwrap(), Kind::RetiredBuffer);
    let (messages, _) = drain(&peer);
    assert!(messages.contains(&message(first, 0, &[])));
    p.event(message(first, 0, &[])).unwrap();
    p.event(message(DISPLAY, 1, &[first])).unwrap();
    assert_eq!(p.client.kind(first).unwrap(), Kind::Free);
}

#[test]
fn release_before_done_still_waits_and_matching_buffer_is_reused() {
    let (mut p, peer) = fixture();
    configure(&mut p, 100, 100);
    p.draw().unwrap();
    drain(&peer);
    let id = p.client.buffers()[0].id();
    p.event(message(id, 0, &[])).unwrap();
    assert!(p.event(message(id, 0, &[])).is_err(), "duplicate release");
    configure(&mut p, 100, 100);
    p.draw().unwrap();
    assert!(p.dirty);
    done(&mut p);
    p.draw().unwrap();
    let (_, files) = drain(&peer);
    assert!(files.is_empty());
    assert_eq!(p.client.buffers().len(), 1);
    assert_eq!(p.client.buffers()[0].id(), id);
}

#[test]
fn invalid_events_are_errors_and_ids_wait_for_delete() {
    let (mut p, peer) = fixture();
    drain(&peer);
    assert!(p.event(message(DISPLAY, 1, &[SHM])).is_err());
    assert!(p.event(message(127, 0, &[])).is_err());
    assert!(p.event(message(u32::MAX, 0, &[])).is_err());
    assert!(p.event(message(TOPLEVEL, 0, &[u32::MAX, 1, 0])).is_err());
    assert!(p.event(message(TOPLEVEL, 0, &[1, 1, 3])).is_err());
    assert!(p
        .event(message(WM, 0, &[1, 2]))
        .unwrap_err()
        .contains("trailing"));
    let mut b = Builder::new();
    b.u32(TOPLEVEL);
    b.u32(3);
    b.string("bad role").unwrap();
    let error = wire::take(&mut b.message(DISPLAY, 0).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        p.event(error).unwrap_err(),
        "Wayland protocol error on object 9, code 3: \"bad role\""
    );
    configure(&mut p, 1, 1);
    assert_eq!(p.size, (1, 1));
    // An axis past the raster's ceiling is refused before any request.
    configure(&mut p, 8193, 1);
    drain(&peer);
    assert!(p.draw().is_err());
    assert!(p.client.buffers().is_empty());
    assert!(drain(&peer).0.is_empty());
    let id = p.client.allocate(Mark::Live).unwrap();
    p.client.set_tag(id, Mark::Gone).unwrap();
    assert_ne!(p.client.allocate(Mark::Live).unwrap(), id);
    p.event(message(DISPLAY, 1, &[id])).unwrap();
    assert_eq!(p.client.allocate(Mark::Live).unwrap(), id);
    while p.client.allocate(Mark::Live).is_ok() {}
    assert!(p
        .client
        .allocate(Mark::Live)
        .unwrap_err()
        .contains("object budget"));
}

#[test]
fn missing_low_version_removed_and_excessive_globals_are_named() {
    let (a, _b) = pair();
    let mut p = Probe::new(a);
    p.event(global(1, "wl_compositor", 3)).unwrap();
    assert!(p
        .event(message(SYNC, 0, &[0]))
        .unwrap_err()
        .contains("wl_compositor v4"));
    assert!(!p.client.bound());
    let (mut p, _b) = fixture();
    assert!(p
        .event(message(REGISTRY, 1, &[30]))
        .unwrap_err()
        .contains("removed"));
    assert!(p.event(global(20, "duplicate", 1)).is_err());
    assert!(p.event(global(0, "zero id", 1)).is_err());
    assert!(p.event(global(21, "zero version", 0)).is_err());
    assert!(p.event(global(22, &"n".repeat(257), 1)).is_err());
    for n in 1000..1124 {
        p.event(global(n, "optional", 1)).unwrap();
    }
    assert!(p.event(global(2000, "one too many", 1)).is_err());
}

#[test]
fn registry_lookups_name_the_lowest_global_and_removal_reports_requirement() {
    let (mut p, peer) = fixture();
    drain(&peer);
    assert_eq!(p.client.find_global("wl_compositor", 4), Some((30, 6)));
    assert_eq!(p.client.find_global("wl_compositor", 7), None);
    assert_eq!(p.client.global_name(70), Some("ignored_optional"));
    assert_eq!(p.client.required(), [30, 20, 90]);
    assert!(p.client.is_required(30) && !p.client.is_required(70));
    p.event(global(60, "ignored_optional", 43)).unwrap();
    let id = p.client.allocate(Mark::Live).unwrap();
    p.client.bind("ignored_optional", 42, id).unwrap();
    assert_eq!(p.client.required(), [30, 20, 90, 60]);
    let (messages, _) = drain(&peer);
    assert_eq!(messages.len(), 1);
    assert_eq!(
        bind_target(&messages[0]),
        ("ignored_optional".into(), 42, id)
    );
    let mut c = Cursor::new(&messages[0].payload);
    assert_eq!(c.u32().unwrap(), 60, "the lowest global offering v42");
    assert!(p
        .client
        .bind("ignored_optional", 44, id)
        .unwrap_err()
        .contains("ignored_optional v44"));
    assert!(matches!(
        p.client.handle(&message(REGISTRY, 1, &[70]), 0).unwrap(),
        Handled::GlobalRemoved {
            id: 70,
            required: false
        }
    ));
    assert_eq!(p.client.global_name(70), None);
    assert!(matches!(
        p.client.handle(&message(REGISTRY, 1, &[60]), 0).unwrap(),
        Handled::GlobalRemoved {
            id: 60,
            required: true
        }
    ));
    assert_eq!(p.client.global_name(60), Some("ignored_optional"));
    p.client.forget_global(60);
    assert_eq!(p.client.global_name(60), None);
    assert_eq!(p.client.required(), [30, 20, 90]);
    assert!(p.event(message(REGISTRY, 1, &[60, 1])).is_err());
}

#[test]
fn consumer_objects_are_handed_back_untouched_and_retire_through_their_tag() {
    let (mut p, peer) = fixture();
    drain(&peer);
    let live = p.client.allocate(Mark::Live).unwrap();
    assert_eq!(live, 10, "the first dynamic id");
    assert!(matches!(
        p.client.handle(&message(live, 3, &[1, 2]), 0).unwrap(),
        Handled::Unhandled
    ));
    p.event(message(live, 3, &[1, 2])).unwrap();
    p.event(message(live, 0, &[])).unwrap();
    assert_eq!(p.unhandled, 2);
    assert!(p.event(message(DISPLAY, 1, &[live])).is_err());
    p.client.set_tag(live, Mark::Gone).unwrap();
    assert_eq!(p.client.kind(live).unwrap(), Kind::App(Mark::Gone));
    p.event(message(DISPLAY, 1, &[live])).unwrap();
    assert_eq!(p.client.kind(live).unwrap(), Kind::Free);
    assert!(p.client.kind(u32::MAX).is_err());
    assert!(p.client.set_tag(u32::MAX, Mark::Gone).is_err());
    // Neither a freed slot nor the client's own can be re-tagged.
    assert!(p.client.set_tag(live, Mark::Gone).is_err());
    assert!(p.client.set_tag(SURFACE, Mark::Gone).is_err());
    assert_eq!(p.client.kind(SURFACE).unwrap(), Kind::Fixed);
    assert!(drain(&peer).0.is_empty());
}

#[test]
fn ping_is_serviced_while_frame_waits_and_close_stops_presentation() {
    let (mut p, peer) = fixture();
    configure(&mut p, 80, 80);
    p.draw().unwrap();
    drain(&peer);
    p.event(message(WM, 0, &[1234])).unwrap();
    let (messages, _) = drain(&peer);
    assert_eq!(messages, [message(WM, 3, &[1234])]);
    p.event(message(TOPLEVEL, 1, &[])).unwrap();
    assert!(p.client.closed());
    assert!(!p.client.can_present());
    done(&mut p);
    p.dirty = true;
    p.draw().unwrap();
    assert!(p.dirty);
    assert!(drain(&peer).0.is_empty());
}

#[test]
fn a_later_free_matching_buffer_is_preferred_over_replacing_the_first() {
    let (mut p, peer) = fixture();
    configure(&mut p, 100, 100);
    p.draw().unwrap();
    drain(&peer);
    done(&mut p);
    configure(&mut p, 200, 100);
    p.draw().unwrap();
    drain(&peer);
    done(&mut p);
    let first = p.client.buffers()[0].id();
    let matching = p.client.buffers()[1].id();
    p.event(message(first, 0, &[])).unwrap();
    p.event(message(matching, 0, &[])).unwrap();
    configure(&mut p, 200, 100);
    p.draw().unwrap();
    let (messages, files) = drain(&peer);
    assert!(files.is_empty());
    assert!(messages.contains(&message(SURFACE, 1, &[matching, 0, 0])));
    assert!(!p.client.buffers()[0].busy());
    assert!(p.client.buffers()[1].busy());
}

#[test]
fn presentation_waits_for_configure_xrgb_and_the_frame_callback() {
    let (a, peer) = pair();
    let mut p = Probe::new(a);
    assert!(!p.client.can_present());
    for event in [
        global(1, "wl_compositor", 4),
        global(2, "wl_shm", 1),
        global(3, "xdg_wm_base", 1),
    ] {
        p.event(event).unwrap();
    }
    p.event(message(SYNC, 0, &[0])).unwrap();
    assert!(p.client.bound() && !p.client.configured());
    configure(&mut p, 64, 64);
    assert!(
        p.client.configured() && !p.client.can_present(),
        "no XRGB yet"
    );
    p.draw().unwrap();
    assert!(p.client.buffers().is_empty() && p.dirty);
    p.event(message(SHM, 0, &[0x3432_5258])).unwrap();
    assert!(!p.client.can_present(), "an unknown format is not XRGB");
    p.event(message(SHM, 0, &[1])).unwrap();
    assert!(p.client.can_present());
    p.draw().unwrap();
    assert!(!p.dirty && p.client.frame_callback().is_some());
    assert!(!p.client.can_present(), "one frame in flight");
    done(&mut p);
    assert!(p.client.can_present());
    p.client.unconfigure();
    assert!(!p.client.configured() && !p.client.can_present());
    configure(&mut p, 64, 64);
    assert!(p.client.can_present());
    drain(&peer);
    assert!(
        p.client.present(0, 0, &mut |_| Ok(())).is_err(),
        "an empty surface is refused"
    );
    assert!(p
        .client
        .present(8, 8, &mut |_| Err("paint refused".into()))
        .unwrap_err()
        .contains("paint refused"));
    let (messages, files) = drain(&peer);
    assert_eq!(files.len(), 1, "the buffer exists before the paint");
    assert!(
        messages
            .iter()
            .all(|m| m.object != SURFACE && m.object != XDG_SURFACE),
        "nothing attached or committed for a refused paint"
    );
    assert!(p.client.frame_callback().is_none());
    assert_eq!(p.client.buffers().len(), 2);
    assert!(!p.client.buffers()[1].busy());
}

#[test]
fn hidden_surface_waits_for_visibility_without_a_callback_deadline() {
    let (mut p, peer) = fixture();
    p.client
        .connection()
        .set_startup_deadline(Some(Instant::now() + INITIAL_DEADLINE));
    configure(&mut p, 100, 100);
    p.draw().unwrap();
    drain(&peer);
    assert!(p.client.connection().startup_deadline().is_none());
    assert!(p.client.frame_callback().is_some());
    assert_eq!(p.frames, 0);
    assert_eq!(
        p.client.connection().budget(WRITE_DEADLINE).unwrap(),
        WRITE_DEADLINE
    );
    p.event(message(WM, 0, &[9])).unwrap();
    assert_eq!(drain(&peer).0, [message(WM, 3, &[9])]);
    p.event(message(TOPLEVEL, 1, &[])).unwrap();
    assert!(p.client.closed());
}

#[test]
fn pointer_image_is_one_immutable_argb_pool_and_follows_enter_serials() {
    let (mut p, peer, _, _, pointer) = seat_fixture();
    p.event(message(pointer, 0, &[19, SURFACE, 512, 768]))
        .unwrap();
    assert_eq!(
        p.pointer,
        [pointer::Event::Enter {
            serial: 19,
            surface: SURFACE,
            x: 512,
            y: 768
        }]
    );
    assert_eq!(p.client.entered(), Some(19));
    assert!(p.client.cursor().is_none(), "nothing without ARGB");
    assert!(drain(&peer).0.is_empty());
    // ARGB advertised while the pointer is inside shows the image at the
    // enter serial.
    p.event(message(SHM, 0, &[0])).unwrap();
    let (surface, buffer) = p.client.cursor().unwrap();
    let (requests, files) = drain(&peer);
    assert_eq!(files.len(), 1);
    let pool = requests
        .iter()
        .find(|request| (request.object, request.opcode) == (SHM, 0))
        .map(|request| Cursor::new(&request.payload).u32().unwrap())
        .unwrap();
    assert_eq!(
        requests,
        [
            message(COMPOSITOR, 0, &[surface]),
            message(SHM, 0, &[pool, 1536]),
            message(pool, 0, &[buffer, 0, 16, 24, 64, 0]),
            message(pool, 1, &[]),
            message(pointer, 0, &[19, surface, 0, 0]),
            message(surface, 1, &[buffer, 0, 0]),
            message(surface, 2, &[0, 0, 16, 24]),
            message(surface, 6, &[]),
        ]
    );
    assert_eq!(p.client.kind(pool).unwrap(), Kind::Retired);
    assert_eq!(p.client.kind(surface).unwrap(), Kind::CursorSurface);
    assert_eq!(p.client.kind(buffer).unwrap(), Kind::CursorBuffer);
    let file = &files[0];
    assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
    assert_eq!(file.metadata().unwrap().nlink(), 0);
    assert_eq!(file.metadata().unwrap().len(), 1536);
    let mut bytes = [0; 1536];
    file.read_exact_at(&mut bytes, 0).unwrap();
    assert_eq!(bytes, cursor_pixels());
    assert!(p.client.buffers().is_empty());
    p.event(message(SHM, 0, &[1])).unwrap();
    p.event(message(SHM, 0, &[0])).unwrap();
    assert!(drain(&peer).0.is_empty());
    p.event(message(surface, 0, &[1])).unwrap();
    p.event(message(surface, 1, &[1])).unwrap();
    p.event(message(buffer, 0, &[])).unwrap();
    assert!(p.event(message(buffer, 0, &[])).is_err());
    p.event(message(DISPLAY, 1, &[pool])).unwrap();
    assert_eq!(p.client.kind(pool).unwrap(), Kind::Free);
    // Leave clears the serial; re-entering only re-sends `set_cursor`.
    p.event(message(pointer, 1, &[20, SURFACE])).unwrap();
    assert_eq!(p.client.entered(), None);
    assert_eq!(p.pointer.last(), Some(&pointer::Event::Leave(SURFACE)));
    p.event(message(pointer, 0, &[21, SURFACE, 0, 0])).unwrap();
    assert_eq!(p.client.entered(), Some(21));
    let (requests, files) = drain(&peer);
    assert!(files.is_empty());
    assert_eq!(requests, [message(pointer, 0, &[21, surface, 0, 0])]);
    assert_eq!(p.client.cursor(), Some((surface, buffer)));
    // Motion, buttons, axes and frames are handed on decoded; enter and
    // leave for another surface end the connection.
    p.event(message(pointer, 2, &[0, 256, 512])).unwrap();
    p.event(message(pointer, 3, &[22, 0, 0x110, 1])).unwrap();
    p.event(message(pointer, 4, &[0, 0, 768])).unwrap();
    p.event(message(pointer, 5, &[])).unwrap();
    assert_eq!(
        &p.pointer[3..],
        [
            pointer::Event::Motion(256, 512),
            pointer::Event::Button {
                serial: 22,
                button: 0x110,
                pressed: true
            },
            pointer::Event::Axis(0, 768),
            pointer::Event::Frame,
        ]
    );
    assert!(p.event(message(pointer, 1, &[23, 99])).is_err());
    assert!(p.event(message(pointer, 0, &[24, 99, 0, 0])).is_err());
    assert_eq!(
        p.client.entered(),
        Some(21),
        "a refused event changes nothing"
    );
}

#[test]
fn the_seat_is_the_lowest_v5_global_capped_at_v7_and_its_devices_follow_capabilities() {
    let (a, peer) = pair();
    let mut p = Probe::new(a);
    for event in [
        global(1, "wl_compositor", 4),
        global(2, "wl_shm", 1),
        global(3, "xdg_wm_base", 1),
        global(4, "wl_seat", 4),
        global(5, "wl_seat", 6),
        global(6, "wl_seat", 10),
    ] {
        p.event(event).unwrap();
    }
    p.event(message(SYNC, 0, &[0])).unwrap();
    let seat = p.client.seat().unwrap();
    assert_eq!(seat, 10, "the seat is the first dynamic id");
    assert_eq!(p.client.required(), [1, 2, 3, 5]);
    let (requests, _) = drain(&peer);
    let binding = requests.iter().rposition(|m| m.object == REGISTRY).unwrap();
    assert_eq!(bind_target(&requests[binding]), ("wl_seat".into(), 6, seat));
    let toplevel = requests
        .iter()
        .position(|m| *m == message(XDG_SURFACE, 1, &[TOPLEVEL]))
        .unwrap();
    assert!(toplevel < binding, "the fixed ids come first");
    assert!(p.client.keyboard().is_none() && p.client.pointer().is_none());
    // Capabilities create the pointer, then the keyboard, once each.
    p.event(message(seat, 0, &[3])).unwrap();
    let (keyboard, pointer) = (p.client.keyboard().unwrap(), p.client.pointer().unwrap());
    assert_eq!((pointer, keyboard), (11, 12));
    assert_eq!(
        drain(&peer).0,
        [message(seat, 0, &[pointer]), message(seat, 1, &[keyboard])]
    );
    assert_eq!(p.capabilities, [(true, true)]);
    p.event(message(seat, 0, &[3])).unwrap();
    assert!(drain(&peer).0.is_empty());
    assert_eq!(
        (p.client.keyboard(), p.client.pointer()),
        (Some(keyboard), Some(pointer))
    );
    assert_eq!(p.capabilities, [(true, true), (true, true)]);
    // A name within the registry's name budget is accepted; over it, or an
    // unknown opcode, ends the connection.
    p.event(text_event(seat, 1, &"s".repeat(NAME_BYTES)))
        .unwrap();
    assert!(p
        .event(text_event(seat, 1, &"s".repeat(NAME_BYTES + 1)))
        .is_err());
    assert!(p.event(message(seat, 2, &[])).is_err());
    // Trailing payload bytes are refused on the seat's name and on a
    // registry removal, before any release.
    let mut trailing = text_event(seat, 1, "seat0");
    trailing.payload.extend_from_slice(&[0; 4]);
    assert!(p.event(trailing).is_err(), "trailing bytes");
    assert!(
        p.event(message(REGISTRY, 1, &[5, 0])).is_err(),
        "a removal with trailing bytes"
    );
    assert!(p.client.seat().is_some(), "refused before any release");
    // Without a v5 seat there is no seat, and the probe is still bound.
    let (a, _peer) = pair();
    let mut p = Probe::new(a);
    for event in [
        global(1, "wl_compositor", 4),
        global(2, "wl_shm", 1),
        global(3, "xdg_wm_base", 1),
        global(4, "wl_seat", 4),
    ] {
        p.event(event).unwrap();
    }
    p.event(message(SYNC, 0, &[0])).unwrap();
    assert!(p.client.seat().is_none() && p.client.bound());
    assert_eq!(p.client.required(), [1, 2, 3]);
}

#[test]
fn a_keymap_crosses_the_socket_and_presses_translate_after_focus_and_the_snapshot() {
    let (mut p, peer, _, keyboard, _) = seat_fixture();
    assert!(p
        .client
        .needs_descriptor(&message(keyboard, 0, &[1, 4]))
        .unwrap());
    assert!(!p
        .client
        .needs_descriptor(&message(keyboard, 3, &[1, 0, 30, 1]))
        .unwrap());
    assert!(p
        .client
        .needs_descriptor(&message(keyboard, 0, &[1]))
        .is_err());
    assert!(!p
        .client
        .needs_descriptor(&message(0xff00_0000, 0, &[]))
        .unwrap());
    send_map(&mut p, &peer, keyboard, 1, &map_file());
    assert_eq!(p.keyboard, [KeyboardEvent::Keymap(Ok(()))]);
    assert!(p.client.input().map.is_some() && !p.client.input().focused);
    assert_eq!(p.client.descriptors(), 0);
    // A press before focus, a held key installed by enter, and a press
    // before the modifier snapshot type nothing.
    press(&mut p, keyboard, 1, 30);
    release(&mut p, keyboard, 2, 30);
    p.event(message(keyboard, 1, &[3, SURFACE, 4, 30])).unwrap();
    assert_eq!(p.keyboard.last(), Some(&KeyboardEvent::Focus(true)));
    assert!(p.client.input().focused && !p.client.input().synchronized);
    press(&mut p, keyboard, 4, 48);
    assert_eq!(p.keyboard.len(), 2);
    release(&mut p, keyboard, 4, 48);
    p.event(message(keyboard, 4, &[5, 0, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.last(), Some(&KeyboardEvent::Ready));
    p.event(message(keyboard, 4, &[6, 0, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.len(), 3, "later snapshots are silent");
    p.event(message(keyboard, 5, &[25, 600])).unwrap();
    press(&mut p, keyboard, 7, 30);
    assert_eq!(p.keyboard.len(), 3, "the key held at enter is still down");
    release(&mut p, keyboard, 8, 30);
    press(&mut p, keyboard, 9, 30);
    assert_eq!(
        p.keyboard.last(),
        Some(&KeyboardEvent::Key {
            serial: 9,
            key: 30,
            stroke: td_ui::keyboard::Stroke {
                chord: "a".into(),
                repeat: true
            }
        })
    );
    // The consumer arms repeat at its clock; the wait follows the due time.
    p.client.arm(30, 100);
    assert_eq!(p.client.wait_ms(650), 50);
    assert!(p.client.repeat(699).unwrap().is_none());
    assert_eq!(p.client.repeat(700).unwrap().unwrap().chord, "a");
    assert_eq!(p.client.wait_ms(700), 40);
    p.client.cancel_repeat();
    assert!(p.client.repeat(5000).unwrap().is_none());
    // A changed snapshot translates the next press under it, and the
    // timing event retimes an armed repeat from the consumer's clock.
    p.event(message(keyboard, 4, &[10, 1, 0, 0, 0])).unwrap();
    release(&mut p, keyboard, 11, 30);
    press(&mut p, keyboard, 12, 48);
    let Some(KeyboardEvent::Key {
        key: 48, stroke, ..
    }) = p.keyboard.last()
    else {
        panic!("{:?}", p.keyboard.last());
    };
    assert_eq!(stroke.chord, "B");
    p.client.arm(48, 1000);
    p.clock = 1000;
    p.event(message(keyboard, 5, &[10, 200])).unwrap();
    assert!(p.client.repeat(1199).unwrap().is_none());
    assert_eq!(p.client.repeat(1200).unwrap().unwrap().chord, "B");
    // Leave clears focus, the held keys and the repeat; presses wait for
    // the next enter and snapshot.
    p.event(message(keyboard, 2, &[13, SURFACE])).unwrap();
    assert_eq!(p.keyboard.last(), Some(&KeyboardEvent::Focus(false)));
    assert!(!p.client.input().focused && !p.client.input().synchronized);
    assert!(p.client.repeat(9000).unwrap().is_none());
    let seen = p.keyboard.len();
    press(&mut p, keyboard, 14, 30);
    assert_eq!(p.keyboard.len(), seen);
    p.event(message(keyboard, 1, &[15, SURFACE, 0])).unwrap();
    p.event(message(keyboard, 4, &[16, 0, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.last(), Some(&KeyboardEvent::Ready));
}

#[test]
fn held_roles_are_reported_once_per_change_while_the_keyboard_is_ready() {
    let (mut p, peer, _, keyboard, _) = seat_fixture();
    send_map(&mut p, &peer, keyboard, 1, &map_file());
    let alt = Held {
        alt: true,
        ..Held::default()
    };
    // Before the map is ready a snapshot reports nothing.
    p.event(message(keyboard, 4, &[1, 8, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.last(), Some(&KeyboardEvent::Keymap(Ok(()))));
    p.event(message(keyboard, 1, &[2, SURFACE, 0])).unwrap();
    // The Ready snapshot is Ready and the baseline: the same roles again
    // are no report, a release from them is one.
    p.event(message(keyboard, 4, &[3, 8, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.last(), Some(&KeyboardEvent::Ready));
    let seen = p.keyboard.len();
    p.event(message(keyboard, 4, &[4, 8, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.len(), seen);
    p.event(message(keyboard, 4, &[4, 0, 0, 0, 0])).unwrap();
    assert_eq!(
        p.keyboard.last(),
        Some(&KeyboardEvent::Held(Held::default()))
    );
    p.event(message(keyboard, 4, &[4, 8, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.last(), Some(&KeyboardEvent::Held(alt)));
    // The same state again is no report; a change is one.
    let seen = p.keyboard.len();
    p.event(message(keyboard, 4, &[5, 8, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.len(), seen);
    p.event(message(keyboard, 4, &[6, 12, 0, 0, 0])).unwrap();
    assert_eq!(
        p.keyboard.last(),
        Some(&KeyboardEvent::Held(Held {
            control: true,
            ..alt
        }))
    );
    // A state the map refuses holds nothing.
    p.event(message(keyboard, 4, &[7, 64 | 8, 0, 0, 0]))
        .unwrap();
    assert_eq!(
        p.keyboard.last(),
        Some(&KeyboardEvent::Held(Held::default()))
    );
    p.event(message(keyboard, 4, &[8, 8, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.last(), Some(&KeyboardEvent::Held(alt)));
    // Leaving clears the roles without a report: the consumer takes
    // `Focus(false)` as its cue, and the next Ready starts from none.
    p.event(message(keyboard, 2, &[9, SURFACE])).unwrap();
    assert_eq!(p.keyboard.last(), Some(&KeyboardEvent::Focus(false)));
    p.event(message(keyboard, 1, &[10, SURFACE, 0])).unwrap();
    p.event(message(keyboard, 4, &[11, 0, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.last(), Some(&KeyboardEvent::Ready));
    let seen = p.keyboard.len();
    p.event(message(keyboard, 4, &[12, 0, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.len(), seen);
    p.event(message(keyboard, 4, &[13, 8, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.last(), Some(&KeyboardEvent::Held(alt)));
    // A new map clears the roles without a report, and its Ready snapshot
    // is the baseline again: Alt held across it reports its release.
    send_map(&mut p, &peer, keyboard, 1, &map_file());
    p.event(message(keyboard, 4, &[14, 8, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.last(), Some(&KeyboardEvent::Ready));
    let seen = p.keyboard.len();
    p.event(message(keyboard, 4, &[15, 8, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.len(), seen);
    p.event(message(keyboard, 4, &[16, 0, 0, 0, 0])).unwrap();
    assert_eq!(
        p.keyboard.last(),
        Some(&KeyboardEvent::Held(Held::default()))
    );
}

#[test]
fn a_refused_keymap_disables_input_and_keyboard_events_are_schema_checked() {
    let (mut p, peer, _, keyboard, _) = seat_fixture();
    send_map(&mut p, &peer, keyboard, 1, &map_file());
    p.event(message(keyboard, 1, &[1, SURFACE, 0])).unwrap();
    p.event(message(keyboard, 4, &[2, 0, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.last(), Some(&KeyboardEvent::Ready));
    // A map the compiler refuses replaces the old one with none; focus
    // stays, the snapshot is awaited again.
    let invalid = backing_file(&std::env::temp_dir(), 4).unwrap();
    send_map(&mut p, &peer, keyboard, 1, &invalid);
    let Some(KeyboardEvent::Keymap(Err(_))) = p.keyboard.last() else {
        panic!("{:?}", p.keyboard.last());
    };
    assert!(p.client.input().map.is_none());
    assert!(p.client.input().focused && !p.client.input().synchronized);
    assert_eq!(p.client.descriptors(), 0);
    press(&mut p, keyboard, 3, 30);
    p.event(message(keyboard, 4, &[4, 0, 0, 0, 0])).unwrap();
    let seen = p.keyboard.len();
    release(&mut p, keyboard, 5, 30);
    press(&mut p, keyboard, 6, 30);
    assert_eq!(p.keyboard.len(), seen, "no map, no strokes");
    // An unsupported format drops its right unread and names the format.
    send_map(&mut p, &peer, keyboard, 2, &map_file());
    let Some(KeyboardEvent::Keymap(Err(detail))) = p.keyboard.last() else {
        panic!("{:?}", p.keyboard.last());
    };
    assert_eq!(detail, "unsupported keymap format 2");
    assert_eq!(p.client.descriptors(), 0);
    // A valid map restores input after the snapshot.
    send_map(&mut p, &peer, keyboard, 1, &map_file());
    assert_eq!(p.keyboard.last(), Some(&KeyboardEvent::Keymap(Ok(()))));
    p.event(message(keyboard, 4, &[7, 0, 0, 0, 0])).unwrap();
    assert_eq!(p.keyboard.last(), Some(&KeyboardEvent::Ready));
    release(&mut p, keyboard, 8, 30);
    press(&mut p, keyboard, 9, 30);
    let Some(KeyboardEvent::Key { key: 30, .. }) = p.keyboard.last() else {
        panic!("{:?}", p.keyboard.last());
    };
    // A keymap without its right is a parked event, never a guess.
    assert!(p.event(message(keyboard, 0, &[1, 4])).is_err());
    // Schema refusals: another surface, a held-key array over budget, a
    // key state past released, a negative rate and an unknown opcode.
    assert!(p.event(message(keyboard, 1, &[1, 99, 0])).is_err());
    assert!(p.event(message(keyboard, 2, &[1, 99])).is_err());
    let mut body = Builder::new();
    body.u32(1);
    body.u32(SURFACE);
    body.u32(769 * 4);
    for _ in 0..769 {
        body.u32(1);
    }
    let crowded = wire::take(&mut body.message(keyboard, 1).unwrap())
        .unwrap()
        .unwrap();
    assert!(p.event(crowded).is_err());
    assert!(p.event(message(keyboard, 3, &[1, 0, 30, 2])).is_err());
    assert!(p.event(message(keyboard, 5, &[u32::MAX, 0])).is_err());
    assert!(p.event(message(keyboard, 6, &[])).is_err());
    assert!(p.event(message(keyboard, 3, &[1, 0, 30])).is_err());
    assert!(p.client.input().map.is_some(), "refusals change nothing");
}

#[test]
fn capability_loss_releases_a_device_and_retired_devices_drain_until_delete_id() {
    let (mut p, peer, seat, keyboard, pointer) = seat_fixture();
    p.event(message(pointer, 0, &[19, SURFACE, 0, 0])).unwrap();
    assert_eq!(p.client.entered(), Some(19));
    p.event(message(seat, 0, &[2])).unwrap();
    assert_eq!(drain(&peer).0, [message(pointer, 1, &[])]);
    assert_eq!(p.capabilities.last(), Some(&(true, false)));
    assert!(p.client.pointer().is_none() && p.client.entered().is_none());
    assert_eq!(p.client.kind(pointer).unwrap(), Kind::RetiredPointer);
    let seen = p.pointer.len();
    p.event(message(pointer, 2, &[0, 256, 256])).unwrap();
    assert_eq!(p.pointer.len(), seen, "retired events are drained");
    assert!(
        p.event(message(pointer, 3, &[0, 0, 0x110, 2])).is_err(),
        "and still schema-checked"
    );
    p.event(message(seat, 0, &[3])).unwrap();
    let replacement = p.client.pointer().unwrap();
    assert_ne!(replacement, pointer, "no reuse before delete_id");
    assert_eq!(drain(&peer).0, [message(seat, 0, &[replacement])]);
    p.event(message(DISPLAY, 1, &[pointer])).unwrap();
    assert_eq!(p.client.kind(pointer).unwrap(), Kind::Free);
    // Keyboard loss releases it, forgets its state and drops the right of
    // a keymap still in flight.
    send_map(&mut p, &peer, keyboard, 1, &map_file());
    p.event(message(keyboard, 1, &[1, SURFACE, 0])).unwrap();
    assert!(p.client.input().map.is_some() && p.client.input().focused);
    p.event(message(seat, 0, &[1])).unwrap();
    assert_eq!(drain(&peer).0, [message(keyboard, 0, &[])]);
    assert_eq!(p.capabilities.last(), Some(&(false, true)));
    assert!(p.client.keyboard().is_none());
    assert!(p.client.input().map.is_none() && !p.client.input().focused);
    let seen = p.keyboard.len();
    send_map(&mut p, &peer, keyboard, 1, &map_file());
    assert_eq!(p.keyboard.len(), seen);
    assert_eq!(p.client.descriptors(), 0);
    assert!(p.client.input().map.is_none());
    p.event(message(seat, 0, &[3])).unwrap();
    assert_ne!(p.client.keyboard(), Some(keyboard));
    p.event(message(DISPLAY, 1, &[keyboard])).unwrap();
    assert_eq!(p.client.kind(keyboard).unwrap(), Kind::Free);
    assert!(p.event(message(keyboard, 0, &[1, 4])).is_err());
    // Losing a device that was never created is silent.
    let (mut p, peer, seat, _, _) = seat_fixture();
    p.event(message(seat, 0, &[0])).unwrap();
    p.event(message(seat, 0, &[0])).unwrap();
    let (requests, _) = drain(&peer);
    assert_eq!(requests.len(), 2, "one release each, once");
    assert_eq!(
        p.capabilities,
        [(true, true), (false, false), (false, false)]
    );
}

#[test]
fn seat_removal_releases_both_devices_and_the_seat_and_forgets_the_global() {
    let (mut p, peer, seat, keyboard, pointer) = seat_fixture();
    send_map(&mut p, &peer, keyboard, 1, &map_file());
    p.event(message(pointer, 0, &[19, SURFACE, 0, 0])).unwrap();
    p.event(message(REGISTRY, 1, &[4])).unwrap();
    assert_eq!(p.seat_removed, 1);
    assert_eq!(
        drain(&peer).0,
        [
            message(keyboard, 0, &[]),
            message(pointer, 1, &[]),
            message(seat, 3, &[])
        ]
    );
    assert!(p.client.seat().is_none());
    assert!(p.client.keyboard().is_none() && p.client.pointer().is_none());
    assert!(p.client.input().map.is_none() && p.client.entered().is_none());
    assert!(!p.client.is_required(4) && p.client.global_name(4).is_none());
    assert_eq!(p.client.required(), [1, 2, 3]);
    assert!(!p.client.closed());
    // Retired objects drain their events, a keymap's right included, until
    // delete_id frees them.
    p.event(message(seat, 0, &[2])).unwrap();
    p.event(text_event(seat, 1, "seat0")).unwrap();
    assert_eq!(p.capabilities.len(), 1);
    send_map(&mut p, &peer, keyboard, 1, &map_file());
    assert_eq!(p.client.descriptors(), 0);
    assert!(p.client.input().map.is_none());
    assert!(drain(&peer).0.is_empty());
    for id in [keyboard, pointer, seat] {
        p.event(message(DISPLAY, 1, &[id])).unwrap();
        assert_eq!(p.client.kind(id).unwrap(), Kind::Free);
    }
    // A seat the client never bound is an ordinary removal, and another
    // required global's removal is still the consumer's error.
    p.event(global(40, "wl_seat", 9)).unwrap();
    p.event(message(REGISTRY, 1, &[40])).unwrap();
    assert_eq!(p.seat_removed, 1);
    assert!(p.event(message(REGISTRY, 1, &[1])).is_err());
}

/// A bound probe with a v10 seat and a v9 data-device manager: `(probe,
/// peer, seat, keyboard, pointer, device)`, the manager's bind checked and
/// the requests so far drained.
fn clipboard_fixture() -> (Probe, UnixStream, u32, u32, u32, u32) {
    let (a, b) = pair();
    let mut p = Probe::new(a);
    for event in [
        global(1, "wl_compositor", 4),
        global(2, "wl_shm", 1),
        global(3, "xdg_wm_base", 1),
        global(4, "wl_seat", 10),
        global(5, "wl_data_device_manager", 9),
    ] {
        p.event(event).unwrap();
    }
    p.event(message(SYNC, 0, &[0])).unwrap();
    p.event(message(DISPLAY, 1, &[SYNC])).unwrap();
    p.event(message(SHM, 0, &[1])).unwrap();
    let seat = p.client.seat().unwrap();
    let (requests, _) = drain(&b);
    let binding = requests.iter().rfind(|m| m.object == REGISTRY).unwrap();
    assert_eq!(
        bind_target(binding),
        ("wl_data_device_manager".into(), 3, 11),
        "the manager follows the seat at the v3 cap"
    );
    assert!(
        requests.contains(&message(11, 1, &[12, seat])),
        "get_data_device"
    );
    assert!(!p.client.is_required(5) && p.client.clipboard());
    p.event(message(seat, 0, &[3])).unwrap();
    let keyboard = p.client.keyboard().unwrap();
    let pointer = p.client.pointer().unwrap();
    assert_eq!((seat, pointer, keyboard), (10, 13, 14));
    drain(&b);
    (p, b, seat, keyboard, pointer, 12)
}

/// A server offer announcing `mimes`, then selected.
fn selection_offer(p: &mut Probe, device: u32, id: u32, mimes: &[&str]) {
    p.event(message(device, 0, &[id])).unwrap();
    for mime in mimes {
        p.event(text_event(id, 0, mime)).unwrap();
    }
    p.event(message(device, 5, &[id])).unwrap();
}

/// The callback id of the one `wl_display.sync` among `requests`.
fn barrier(requests: &[Message]) -> u32 {
    let syncs: Vec<u32> = requests
        .iter()
        .filter(|m| m.object == DISPLAY && m.opcode == 0)
        .map(|m| Cursor::new(&m.payload).u32().unwrap())
        .collect();
    assert_eq!(syncs.len(), 1, "one barrier");
    syncs[0]
}

/// The barrier's callback fires and its id is freed.
fn barrier_done(p: &mut Probe, id: u32) {
    p.event(message(id, 0, &[0])).unwrap();
    p.event(message(DISPLAY, 1, &[id])).unwrap();
    assert_eq!(p.client.kind(id).unwrap(), Kind::Free);
}

/// `wl_data_source.send` for `mime` carrying `file`, across the socket.
fn send_right(p: &mut Probe, peer: &UnixStream, source: u32, mime: &str, file: &File) {
    let mut sender = Connection::new(peer.try_clone().unwrap()).unwrap();
    let mut body = Builder::new();
    body.string(mime).unwrap();
    sender.send(source, 1, body, Some(file)).unwrap();
    p.client.connection().read_more().unwrap();
    while let Some(event) = p.client.connection().take().unwrap() {
        assert!(p.client.needs_descriptor(&event).unwrap());
        p.event(event).unwrap();
    }
}

fn endpoint() -> (UnixStream, File) {
    let (reader, writer) = UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    (reader, File::from(OwnedFd::from(writer)))
}

#[test]
fn the_data_device_is_bound_optionally_at_v3_after_the_seat() {
    let (mut p, peer, _, _, _, _) = clipboard_fixture();
    // A manager advertised after the roundtrip is not bound.
    p.event(global(50, "wl_data_device_manager", 3)).unwrap();
    assert!(drain(&peer).0.is_empty());
    // Below v3, or without a seat, there is no clipboard.
    let (a, b) = pair();
    let mut p = Probe::new(a);
    for event in [
        global(1, "wl_compositor", 4),
        global(2, "wl_shm", 1),
        global(3, "xdg_wm_base", 1),
        global(4, "wl_seat", 10),
        global(5, "wl_data_device_manager", 2),
    ] {
        p.event(event).unwrap();
    }
    p.event(message(SYNC, 0, &[0])).unwrap();
    assert!(p.client.seat().is_some() && !p.client.clipboard());
    assert!(p
        .client
        .offer_selection(1)
        .unwrap_err()
        .contains("no clipboard device"));
    assert!(p.client.selection().is_none() && p.client.source().is_none());
    assert!(!drain(&b)
        .0
        .iter()
        .any(|m| m.object == REGISTRY && bind_target(m).0 == "wl_data_device_manager"));
    let (a, b) = pair();
    let mut p = Probe::new(a);
    for event in [
        global(1, "wl_compositor", 4),
        global(2, "wl_shm", 1),
        global(3, "xdg_wm_base", 1),
        global(5, "wl_data_device_manager", 3),
    ] {
        p.event(event).unwrap();
    }
    p.event(message(SYNC, 0, &[0])).unwrap();
    assert!(p.client.seat().is_none() && !p.client.clipboard());
    let (requests, _) = drain(&b);
    assert_eq!(requests.iter().filter(|m| m.object == REGISTRY).count(), 3);
}

#[test]
fn offers_are_budgeted_and_retired_behind_one_barrier_by_generation() {
    let (mut p, peer, _, _, _, device) = clipboard_fixture();
    // Offer ids are the server's, a live id is not reused, and the offer
    // and announcement budgets hold.
    assert!(p.event(message(device, 0, &[31])).is_err());
    assert!(
        p.event(message(device, 5, &[0xff00_0000])).is_err(),
        "unknown"
    );
    p.event(message(device, 0, &[0xff00_0000])).unwrap();
    assert!(p.event(message(device, 0, &[0xff00_0000])).is_err());
    for _ in 0..ANNOUNCEMENTS {
        p.event(text_event(0xff00_0000, 0, "image/png")).unwrap();
    }
    p.event(text_event(0xff00_0000, 0, UTF8)).unwrap();
    p.event(message(device, 5, &[0xff00_0000])).unwrap();
    assert_eq!(p.clipboard, ["selection"]);
    assert_eq!(p.client.selection(), Some(0xff00_0000));
    assert_eq!(
        p.client.selection_mime(),
        None,
        "over the announcement budget"
    );
    for id in 1..OFFER_LIMIT as u32 {
        p.event(message(device, 0, &[0xff00_0000 + id])).unwrap();
    }
    assert!(
        p.event(message(device, 0, &[0xff00_0100])).is_err(),
        "offer budget"
    );
    assert!(drain(&peer).0.is_empty(), "nothing retired yet");
    // A selection retires every other offer behind one barrier; the
    // explicit UTF-8 spelling is preferred and kept as announced.
    p.event(text_event(0xff00_0001, 0, &"x".repeat(257)))
        .unwrap();
    p.event(text_event(0xff00_0001, 0, "Text/Plain")).unwrap();
    p.event(text_event(0xff00_0001, 0, PLAIN)).unwrap();
    p.event(text_event(0xff00_0001, 0, "TEXT/PLAIN;CHARSET=UTF-8"))
        .unwrap();
    p.event(message(0xff00_0001, 1, &[7])).unwrap();
    p.event(message(0xff00_0001, 2, &[2])).unwrap();
    assert!(
        p.event(message(0xff00_0001, 3, &[])).is_err(),
        "unknown offer event"
    );
    assert!(
        p.event(message(0xff00_0001, 2, &[3])).is_err(),
        "invalid action"
    );
    p.event(message(device, 5, &[0xff00_0001])).unwrap();
    assert_eq!(p.client.selection_mime(), Some("TEXT/PLAIN;CHARSET=UTF-8"));
    let (requests, _) = drain(&peer);
    let destroyed: Vec<u32> = requests
        .iter()
        .filter(|m| m.opcode == 2 && m.object >= 0xff00_0000)
        .map(|m| m.object)
        .collect();
    assert_eq!(destroyed.len(), OFFER_LIMIT - 1);
    assert!(!destroyed.contains(&0xff00_0001));
    let first = barrier(&requests);
    assert_eq!(requests.len(), OFFER_LIMIT);
    // A retired offer's events drain, and its id may return before the
    // barrier as a new generation the old barrier cannot delete.
    p.event(text_event(0xff00_0000, 0, UTF8)).unwrap();
    assert!(
        p.event(message(device, 5, &[0xff00_0000])).is_err(),
        "retired"
    );
    selection_offer(&mut p, device, 0xff00_0000, &[PLAIN]);
    assert_eq!(p.client.selection(), Some(0xff00_0000));
    assert_eq!(
        drain(&peer).0,
        [message(0xff00_0001, 2, &[])],
        "coalesced behind the first barrier"
    );
    barrier_done(&mut p, first);
    assert_eq!(
        p.client.selection_mime(),
        Some(PLAIN),
        "the new generation survives"
    );
    let (requests, _) = drain(&peer);
    let second = barrier(&requests);
    assert_eq!(requests, [message(DISPLAY, 0, &[second])]);
    barrier_done(&mut p, second);
    assert!(drain(&peer).0.is_empty());
    // Rapid reuse withholding the callback: one barrier outstanding, and
    // its callback keeps the last generation.
    for _ in 0..100 {
        p.event(message(device, 5, &[0])).unwrap();
        assert!(p.client.selection().is_none());
        selection_offer(&mut p, device, 0xff00_0000, &[PLAIN]);
    }
    let (requests, _) = drain(&peer);
    assert_eq!(requests.iter().filter(|m| m.opcode == 2).count(), 100);
    let third = barrier(&requests);
    barrier_done(&mut p, third);
    assert!(drain(&peer).0.is_empty(), "nothing left to retire");
    assert_eq!(p.client.selection_mime(), Some(PLAIN));
    assert!(p.clipboard.iter().all(|e| *e == "selection"));
}

#[test]
fn focus_loss_drags_and_a_retired_device_retire_offers_and_the_selection() {
    let (mut p, peer, seat, keyboard, _, device) = clipboard_fixture();
    // A selection before focus is retained; leave retires it.
    selection_offer(&mut p, device, 0xff00_0010, &["text/plain;charset=UTF-8"]);
    assert_eq!(p.client.selection_mime(), Some("text/plain;charset=UTF-8"));
    send_map(&mut p, &peer, keyboard, 1, &map_file());
    p.event(message(keyboard, 1, &[1, SURFACE, 0])).unwrap();
    assert_eq!(p.client.selection(), Some(0xff00_0010));
    drain(&peer);
    p.event(message(keyboard, 2, &[2, SURFACE])).unwrap();
    assert!(p.client.selection().is_none() && p.client.selection_mime().is_none());
    let (requests, _) = drain(&peer);
    let first = barrier(&requests);
    assert_eq!(
        requests,
        [message(0xff00_0010, 2, &[]), message(DISPLAY, 0, &[first])]
    );
    barrier_done(&mut p, first);
    // A drag's offer is retired without accepting or finishing it and is
    // never the selection; a drag over the selection's offer is an error.
    selection_offer(&mut p, device, 0xff00_0010, &[PLAIN]);
    p.event(message(device, 0, &[0xff00_0011])).unwrap();
    p.event(text_event(0xff00_0011, 0, UTF8)).unwrap();
    assert!(
        p.event(message(device, 1, &[0, 99, 0, 0, 0xff00_0011]))
            .is_err(),
        "another surface"
    );
    p.event(message(device, 1, &[0, SURFACE, 0, 0, 0xff00_0011]))
        .unwrap();
    p.event(message(device, 3, &[0, 0, 0])).unwrap();
    p.event(message(device, 2, &[])).unwrap();
    p.event(message(device, 4, &[])).unwrap();
    assert_eq!(p.client.selection(), Some(0xff00_0010));
    let (requests, _) = drain(&peer);
    let second = barrier(&requests);
    assert_eq!(
        requests,
        [message(0xff00_0011, 2, &[]), message(DISPLAY, 0, &[second])]
    );
    assert!(p
        .event(message(device, 1, &[0, SURFACE, 0, 0, 0xff00_0010]))
        .is_err());
    p.event(message(device, 1, &[0, SURFACE, 0, 0, 0])).unwrap();
    assert!(
        p.event(message(device, 6, &[])).is_err(),
        "unknown device event"
    );
    barrier_done(&mut p, second);
    assert!(drain(&peer).0.is_empty());
    // Keyboard loss clears the selection as leave does.
    p.event(message(keyboard, 1, &[3, SURFACE, 0])).unwrap();
    p.event(message(seat, 0, &[1])).unwrap();
    assert!(p.client.selection().is_none());
    let (requests, _) = drain(&peer);
    let third = barrier(&requests);
    assert_eq!(
        requests,
        [
            message(keyboard, 0, &[]),
            message(0xff00_0010, 2, &[]),
            message(DISPLAY, 0, &[third])
        ]
    );
    barrier_done(&mut p, third);
    // The expired drag: an enter naming an offer already dropped at its
    // barrier is ignored, without a request.
    p.event(message(device, 1, &[0, SURFACE, 0, 0, 0xff00_0010]))
        .unwrap();
    assert!(drain(&peer).0.is_empty());
    // The manager's removal releases the device: its later offers are
    // retired at once, its selection and drags are ignored, its events are
    // still checked, and its id waits for delete_id.
    p.event(message(REGISTRY, 1, &[5])).unwrap();
    assert_eq!(p.clipboard.last(), Some(&"released"));
    assert!(!p.client.clipboard());
    assert_eq!(p.client.kind(device).unwrap(), Kind::RetiredDataDevice);
    assert_eq!(drain(&peer).0, [message(device, 2, &[])]);
    p.event(message(device, 0, &[0xff00_0020])).unwrap();
    p.event(text_event(0xff00_0020, 0, PLAIN)).unwrap();
    p.event(message(device, 5, &[0xff00_0020])).unwrap();
    p.event(message(device, 1, &[0, SURFACE, 0, 0, 0xff00_0020]))
        .unwrap();
    assert!(p.client.selection().is_none());
    let (requests, _) = drain(&peer);
    let fourth = barrier(&requests);
    assert_eq!(
        requests,
        [message(0xff00_0020, 2, &[]), message(DISPLAY, 0, &[fourth])]
    );
    assert!(p.event(message(device, 0, &[7])).is_err(), "still checked");
    barrier_done(&mut p, fourth);
    p.event(message(DISPLAY, 1, &[device])).unwrap();
    assert_eq!(p.client.kind(device).unwrap(), Kind::Free);
    p.event(message(REGISTRY, 1, &[5])).unwrap();
    assert_eq!(
        p.clipboard.iter().filter(|e| **e == "released").count(),
        1,
        "a second removal is ordinary"
    );
}

#[test]
fn a_source_offers_both_text_mimes_sends_over_its_right_and_retires() {
    let (mut p, peer, _, _, _, device) = clipboard_fixture();
    let source = p.client.offer_selection(1234).unwrap();
    assert_eq!(p.client.source(), Some(source));
    assert_eq!(
        drain(&peer).0,
        [
            message(11, 0, &[source]),
            text_event(source, 0, UTF8),
            text_event(source, 0, PLAIN),
            message(device, 1, &[source, 1234]),
        ]
    );
    // A supported MIME's right reaches the consumer, which writes its
    // text; an unsupported one's is dropped, exactly it.
    let (mut reader, writer) = endpoint();
    send_right(&mut p, &peer, source, PLAIN, &writer);
    drop(writer);
    assert_eq!(p.clipboard, ["send"]);
    let mut text = String::new();
    reader.read_to_string(&mut text).unwrap();
    assert_eq!(text, "probe text");
    assert_eq!(p.client.descriptors(), 0);
    let (mut reader, writer) = endpoint();
    send_right(&mut p, &peer, source, "Text/Plain;Charset=UTF-8", &writer);
    drop(writer);
    assert_eq!(p.clipboard, ["send", "send"]);
    let mut text = String::new();
    reader.read_to_string(&mut text).unwrap();
    assert_eq!(text, "probe text", "ASCII case ignored");
    let (mut reader, writer) = endpoint();
    send_right(&mut p, &peer, source, "image/png", &writer);
    drop(writer);
    assert_eq!(p.clipboard, ["send", "send"]);
    assert_eq!(reader.read(&mut [0]).unwrap(), 0, "the right was dropped");
    // Other source events are validated and ignored.
    p.event(text_event(source, 0, "x")).unwrap();
    p.event(message(source, 0, &[0])).unwrap();
    p.event(message(source, 3, &[])).unwrap();
    p.event(message(source, 4, &[])).unwrap();
    p.event(message(source, 5, &[1])).unwrap();
    assert!(p.event(message(source, 5, &[3])).is_err());
    assert!(p.event(message(source, 6, &[])).is_err());
    assert!(p.event(text_event(source, 1, "")).is_err(), "empty MIME");
    // A malformed send leaves the FIFO alone; the next send pops its right.
    let (_reader, writer) = std::io::pipe().unwrap();
    td_ui::wayland::peer::push_descriptor(p.client.connection(), writer.into()).unwrap();
    let mut malformed = text_event(source, 1, PLAIN);
    malformed.payload.extend_from_slice(&[0; 4]);
    assert!(p.client.needs_descriptor(&malformed).is_err());
    assert!(p.event(malformed).is_err());
    assert_eq!(p.client.descriptors(), 1);
    p.event(text_event(source, 1, PLAIN)).unwrap();
    assert_eq!(p.client.descriptors(), 0);
    assert_eq!(p.clipboard, ["send", "send", "send"]);
    assert!(
        p.event(text_event(source, 1, PLAIN)).is_err(),
        "a send without its right"
    );
    // Replacement destroys the previous source, whose sends drop their
    // rights and whose cancel is ignored; cancel of the live one retires
    // it, and both ids wait for delete_id.
    let second = p.client.offer_selection(1235).unwrap();
    assert_ne!(second, source);
    assert_eq!(p.client.kind(source).unwrap(), Kind::RetiredDataSource);
    let (requests, _) = drain(&peer);
    assert_eq!(requests.len(), 5);
    assert_eq!(requests.last(), Some(&message(source, 1, &[])));
    let (mut reader, writer) = endpoint();
    send_right(&mut p, &peer, source, UTF8, &writer);
    drop(writer);
    assert_eq!(reader.read(&mut [0]).unwrap(), 0, "retired: dropped");
    p.event(message(source, 2, &[])).unwrap();
    assert_eq!(p.clipboard, ["send", "send", "send"]);
    assert_eq!(p.client.source(), Some(second));
    p.event(message(second, 2, &[])).unwrap();
    assert_eq!(p.clipboard, ["send", "send", "send", "cancelled"]);
    assert!(p.client.source().is_none());
    assert_eq!(p.client.kind(second).unwrap(), Kind::RetiredDataSource);
    assert_eq!(drain(&peer).0, [message(second, 1, &[])]);
    for id in [source, second] {
        p.event(message(DISPLAY, 1, &[id])).unwrap();
        assert_eq!(p.client.kind(id).unwrap(), Kind::Free);
    }
    assert!(p.event(message(source, 2, &[])).is_err(), "freed");
}

#[test]
fn receive_names_the_selections_preferred_spelling_over_the_consumers_endpoint() {
    let (mut p, peer, _, _, _, device) = clipboard_fixture();
    assert!(p.client.receive(&endpoint().1).is_err(), "no selection");
    selection_offer(&mut p, device, 0xff00_0010, &["image/png"]);
    assert!(p.client.receive(&endpoint().1).is_err(), "unsupported");
    selection_offer(
        &mut p,
        device,
        0xff00_0011,
        &[PLAIN, "Text/Plain;Charset=UTF-8"],
    );
    drain(&peer);
    let (mut reader, writer) = endpoint();
    p.client.receive(&writer).unwrap();
    drop(writer);
    let (requests, files) = drain(&peer);
    assert_eq!(
        requests,
        [text_event(0xff00_0011, 1, "Text/Plain;Charset=UTF-8")]
    );
    let mut file = files.into_iter().next().unwrap();
    file.write_all(b"from the peer").unwrap();
    drop(file);
    let mut text = String::new();
    reader.read_to_string(&mut text).unwrap();
    assert_eq!(text, "from the peer");
}

#[test]
fn manager_and_seat_removal_release_the_source_and_device_in_order() {
    let (mut p, peer, _, _, _, device) = clipboard_fixture();
    let source = p.client.offer_selection(1).unwrap();
    selection_offer(&mut p, device, 0xff00_0010, &[UTF8]);
    drain(&peer);
    p.event(message(REGISTRY, 1, &[5])).unwrap();
    assert_eq!(p.clipboard.last(), Some(&"released"));
    assert!(!p.client.clipboard());
    assert!(p.client.source().is_none() && p.client.selection().is_none());
    let (requests, _) = drain(&peer);
    let sync = barrier(&requests);
    assert_eq!(
        requests,
        [
            message(0xff00_0010, 2, &[]),
            message(DISPLAY, 0, &[sync]),
            message(source, 1, &[]),
            message(device, 2, &[]),
        ]
    );
    assert!(p.client.offer_selection(2).is_err());
    assert!(p.client.global_name(5).is_none() && !p.client.closed());
    // The manager has no destructor: its id stays taken and silent.
    assert_eq!(p.client.kind(11).unwrap(), Kind::DataManager);
    assert!(p.event(message(11, 0, &[])).is_err());
    // Seat removal: the keyboard, the pointer, the clipboard, the seat.
    let (mut p, peer, seat, keyboard, pointer, device) = clipboard_fixture();
    let source = p.client.offer_selection(1).unwrap();
    selection_offer(&mut p, device, 0xff00_0010, &[UTF8]);
    drain(&peer);
    p.event(message(REGISTRY, 1, &[4])).unwrap();
    assert_eq!(p.seat_removed, 1);
    assert!(!p.client.clipboard());
    let (requests, _) = drain(&peer);
    let sync = barrier(&requests);
    assert_eq!(
        requests,
        [
            message(keyboard, 0, &[]),
            message(0xff00_0010, 2, &[]),
            message(DISPLAY, 0, &[sync]),
            message(pointer, 1, &[]),
            message(source, 1, &[]),
            message(device, 2, &[]),
            message(seat, 3, &[]),
        ]
    );
    barrier_done(&mut p, sync);
    for id in [keyboard, pointer, source, device, seat] {
        p.event(message(DISPLAY, 1, &[id])).unwrap();
        assert_eq!(p.client.kind(id).unwrap(), Kind::Free);
    }
    p.event(message(REGISTRY, 1, &[5])).unwrap();
    assert!(p.clipboard.iter().all(|e| *e == "selection"));
}

#[test]
fn complete_loop_accepts_split_events_and_closes_cleanly() {
    let (client, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let worker = std::thread::spawn(move || {
        let mut probe = Probe::new(client);
        let result = run(&mut probe);
        (result, probe.frames, probe.client.closed())
    });
    let mut handshake = [0; 24];
    peer.read_exact(&mut handshake).unwrap();
    assert_eq!(
        wire::take(&mut handshake.to_vec()).unwrap().unwrap(),
        message(DISPLAY, 1, &[REGISTRY])
    );
    assert_eq!(
        wire::take(&mut handshake[12..].to_vec()).unwrap().unwrap(),
        message(DISPLAY, 0, &[SYNC])
    );
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
    for chunk in events.chunks(3) {
        peer.write_all(chunk).unwrap();
    }
    // Wait for the first frame's commit, complete its callback, then close.
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
    let (result, frames, closed) = worker.join().unwrap();
    result.unwrap();
    assert_eq!(frames, 1);
    assert!(closed);
}

#[test]
fn the_loop_parks_an_event_until_its_right_arrives_and_keeps_wire_order() {
    let (client, peer) = UnixStream::pair().unwrap();
    let worker = std::thread::spawn(move || {
        let mut probe = Probe::new(client);
        probe.wants_right = true;
        let result = run(&mut probe);
        let wait = probe.client.connection().wait();
        (
            result,
            probe.parked,
            probe.waits,
            probe.rights,
            probe.client.descriptors(),
            wait,
            probe.log,
        )
    });
    let mut sender = Connection::new(peer.try_clone().unwrap()).unwrap();
    for event in [
        global(1, "wl_compositor", 4),
        global(2, "wl_shm", 1),
        global(3, "xdg_wm_base", 1),
        message(SYNC, 0, &[0]),
    ] {
        let mut body = Builder::new();
        for word in event.payload.as_chunks::<4>().0 {
            body.u32(u32::from_ne_bytes(*word));
        }
        sender.send(event.object, event.opcode, body, None).unwrap();
    }
    let start = Instant::now();
    loop {
        assert!(start.elapsed() < Duration::from_secs(2));
        let (messages, _) = drain(&peer);
        if messages.contains(&message(SURFACE, 6, &[])) {
            break;
        }
    }
    // The probe's object is the first dynamic id; its opcode 0 needs a
    // right that has not been sent yet.
    sender.words(10, 0, &[7]).unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let file = backing_file(&std::env::temp_dir(), 16).unwrap();
    let mut ping = Builder::new();
    ping.u32(987);
    sender.send(WM, 0, ping, Some(&file)).unwrap();
    sender.words(10, 1, &[]).unwrap();
    sender.words(TOPLEVEL, 1, &[]).unwrap();
    let (result, parked, waits, rights, pending, wait, log) = worker.join().unwrap();
    result.unwrap();
    assert_eq!(parked, Some(10));
    assert!(
        waits >= 1,
        "the parked event cancelled at least one turn's repeat"
    );
    assert_eq!((rights, pending), (1, 0));
    assert_eq!(
        wait, IDLE_WAIT,
        "every turn starts from the idle wait, not the one a turn shortened"
    );
    assert_eq!(
        &log[4..],
        [(10, 0), (WM, 0), (10, 1), (TOPLEVEL, 1)],
        "the parked event kept its place before the ping that carried its right"
    );
    assert!(drain(&peer).0.contains(&message(WM, 3, &[987])));
}
