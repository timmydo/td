#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The shared client against a scripted peer: the object table and
//! registry, the toplevel's buffers and frame callback, the pointer image,
//! and the turn loop, driven by the smallest consumer. The presentation and
//! loop tests moved here from td-editor's window tests.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use td_ui::client::{
    run, App, Client, Handled, Kind, Tag, BUFFERS, COMPOSITOR, DISPLAY, GLOBALS, INITIAL_DEADLINE,
    MESSAGES_PER_TURN, NAME_BYTES, OBJECTS, REGISTRY, SHM, SURFACE, SYNC, TOPLEVEL, WM,
    XDG_SURFACE,
};
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

/// The smallest consumer: one flat colour behind a marker pixel, a count
/// of what the client hands back, and optionally one object of its own
/// whose opcode 0 carries a right.
struct Probe {
    client: Client<Mark>,
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
        match self.client.handle(&message)? {
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
            Handled::Done | Handled::Format(_) | Handled::GlobalRemoved { .. } => Ok(()),
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
    assert_eq!(
        p.client.handle(&message(REGISTRY, 1, &[70])).unwrap(),
        Handled::GlobalRemoved {
            id: 70,
            required: false
        }
    );
    assert_eq!(p.client.global_name(70), None);
    assert_eq!(
        p.client.handle(&message(REGISTRY, 1, &[60])).unwrap(),
        Handled::GlobalRemoved {
            id: 60,
            required: true
        }
    );
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
    assert_eq!(
        p.client.handle(&message(live, 3, &[1, 2])).unwrap(),
        Handled::Unhandled
    );
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
    let (mut p, peer) = fixture();
    drain(&peer);
    let pointer = p.client.allocate(Mark::Live).unwrap();
    p.client.show_cursor(pointer, 19).unwrap();
    assert!(p.client.cursor().is_none(), "nothing without ARGB");
    assert!(drain(&peer).0.is_empty());
    p.event(message(SHM, 0, &[0])).unwrap();
    p.client.show_cursor(pointer, 19).unwrap();
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
    p.client.show_cursor(pointer, 21).unwrap();
    let (requests, files) = drain(&peer);
    assert!(files.is_empty());
    assert_eq!(requests, [message(pointer, 0, &[21, surface, 0, 0])]);
    assert_eq!(p.client.cursor(), Some((surface, buffer)));
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
