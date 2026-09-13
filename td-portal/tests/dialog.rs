//! The private-Wayland FileChooser dialog driven as a library against an
//! in-test mock compositor: the manager-protocol regression the native render
//! case cannot reach. The mock speaks the private registry and manager
//! protocol over a real socket using the toolkit's own transport, sends the
//! pinned keymap so a physical Return is accepted, checks the frames the
//! shared client submits, and confirms the caller's first-frame checksum
//! equals the pixels the compositor received.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use td_portal::dialog::{spawn, DialogConfig, Notice};
use td_portal::file_chooser::{Mode, Outcome};
use td_portal::keyboard::XKB_KEYMAP;
use td_ui::client::{DISPLAY, SHM, SURFACE, TOPLEVEL, XDG_SURFACE};
use td_ui::wayland::{backing_file, Connection};
use td_ui::wire::{Builder, Cursor, Message};

const REGISTRY: u32 = 2;
const SYNC: u32 = 3;
const XRGB8888: u32 = 1;
const KEYBOARD_CAPABILITY: u32 = 2;
const KEY_PRESSED: u32 = 1;
const DISMISSED: u32 = 2;
const PARENT: &str = "0123456789abcdef";

/// The exact private registry the mock advertises, matching the dialog's
/// EXPECTED_GLOBALS by SET. Each entry carries the registry NAME the compositor
/// assigns: td-compositor's `GLOBAL_*` constants (server.rs) are fixed and NOT
/// monotonic in announcement order, so the mock reproduces that numbering. The
/// shared client stores globals in a name-keyed BTreeMap and yields them in
/// name order, so an *ordered* comparison in `bind_manager` would fail against
/// this faithful mock — the set comparison must not. Announced in the
/// compositor's PUBLIC_GLOBALS order, the privileged manager last.
const GLOBALS: [(u32, &str, u32); 11] = [
    (1, "wl_compositor", 4),
    (7, "wl_subcompositor", 1),
    (2, "wl_shm", 1),
    (3, "wl_output", 4),
    (4, "xdg_wm_base", 1),
    (6, "zxdg_decoration_manager_v1", 1),
    (8, "wl_data_device_manager", 3),
    (9, "zxdg_exporter_v2", 1),
    (10, "zxdg_importer_v2", 1),
    (5, "wl_seat", 7),
    (11, "td_portal_manager_v1", 1),
];

/// FNV-1a over the whole frame, the oracle for the checksum the dialog folds
/// and reports; it must equal the dialog's own `pixel_checksum`.
fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
}

struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Self {
        for attempt in 0..32u8 {
            let path = std::env::temp_dir().join(format!(
                "td-portal-dialog-it-{name}-{}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create test directory: {error}"),
            }
        }
        panic!("exhausted test directory names");
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The mock compositor over the toolkit's own transport: it reads the client's
/// requests and sends events, with the keymap descriptor, the same way td-ui's
/// client does.
struct Peer {
    connection: Connection,
}

impl Peer {
    fn new(stream: UnixStream) -> Self {
        let mut connection = Connection::new(stream).unwrap();
        connection.set_wait(Duration::from_millis(100));
        Self { connection }
    }

    fn next(&mut self, deadline: Instant) -> Message {
        loop {
            if let Some(message) = self.connection.take().unwrap() {
                // A real compositor answers wl_display.sync; the dialog pulses
                // one every ten seconds to defeat the private-peer inactivity
                // timeout. Absorb any such keepalive (done, then delete_id so
                // the client reclaims the callback slot) so it never disturbs
                // the expected request stream. The initial roundtrip's sync
                // (callback SYNC) is answered explicitly by the caller.
                if (message.object, message.opcode) == (DISPLAY, 0) {
                    let callback = Cursor::new(&message.payload).u32().unwrap();
                    if callback != SYNC {
                        let mut done = Builder::new();
                        done.u32(0);
                        self.send(callback, 0, done);
                        let mut deleted = Builder::new();
                        deleted.u32(callback);
                        self.send(DISPLAY, 1, deleted);
                        continue;
                    }
                }
                return message;
            }
            assert!(Instant::now() < deadline, "mock compositor read deadline");
            self.connection.read_more().unwrap();
        }
    }

    fn expect(&mut self, deadline: Instant, object: u32, opcode: u16) -> Message {
        let message = self.next(deadline);
        assert_eq!((message.object, message.opcode), (object, opcode));
        message
    }

    fn send(&mut self, object: u32, opcode: u16, body: Builder) {
        self.connection.send(object, opcode, body, None).unwrap();
    }

    fn send_fd(&mut self, object: u32, opcode: u16, body: Builder, file: &File) {
        self.connection.send(object, opcode, body, Some(file)).unwrap();
    }

    /// Read one frame the client submitted: the pool and its descriptor, the
    /// buffer, the geometry, attach, damage, frame callback and commit. Returns
    /// the buffer id, the frame callback id, and the checksum of the pixels the
    /// client actually wrote, so the caller can compare it to the reported one.
    fn expect_frame(
        &mut self,
        deadline: Instant,
        width: usize,
        height: usize,
        runtime: &Path,
    ) -> (u32, u32, u64) {
        let create_pool = self.expect(deadline, SHM, 0);
        let mut args = Cursor::new(&create_pool.payload);
        let pool = args.u32().unwrap();
        let size = usize::try_from(args.u32().unwrap()).unwrap();
        args.finish().unwrap();
        assert_eq!(size, width * height * 4);
        let descriptor = self.connection.pop_descriptor().expect("wl_shm descriptor");
        let pixels = File::from(descriptor);
        // The shm file lives in the client's runtime directory, not the display
        // directory: this catches a wrong backing path even as root.
        let backing = fs::read_link(format!("/proc/self/fd/{}", pixels.as_raw_fd())).unwrap();
        assert_eq!(
            backing.parent(),
            Some(fs::canonicalize(runtime).unwrap().as_path())
        );

        let create_buffer = self.expect(deadline, pool, 0);
        let mut args = Cursor::new(&create_buffer.payload);
        let buffer = args.u32().unwrap();
        assert_eq!(args.i32().unwrap(), 0);
        assert_eq!(args.i32().unwrap(), i32::try_from(width).unwrap());
        assert_eq!(args.i32().unwrap(), i32::try_from(height).unwrap());
        assert_eq!(args.i32().unwrap(), i32::try_from(width * 4).unwrap());
        assert_eq!(args.u32().unwrap(), XRGB8888);
        args.finish().unwrap();
        self.expect(deadline, pool, 1); // destroy pool
        self.expect(deadline, XDG_SURFACE, 3); // set_window_geometry
        let attach = self.expect(deadline, SURFACE, 1);
        let mut args = Cursor::new(&attach.payload);
        assert_eq!(args.u32().unwrap(), buffer);
        assert_eq!(args.i32().unwrap(), 0);
        assert_eq!(args.i32().unwrap(), 0);
        args.finish().unwrap();
        self.expect(deadline, SURFACE, 9); // damage_buffer
        let frame = self.expect(deadline, SURFACE, 3);
        let mut args = Cursor::new(&frame.payload);
        let callback = args.u32().unwrap();
        args.finish().unwrap();
        self.expect(deadline, SURFACE, 6); // commit

        // The shared client sends the pool descriptor before it renders and
        // writes the pixels (unlike the old hand-rolled transport, which wrote
        // first). Read the frame only after its commit, when the bytes are on
        // the shared file.
        assert_eq!(pixels.metadata().unwrap().len(), size as u64);
        let mut bytes = vec![0; size];
        pixels.read_exact_at(&mut bytes, 0).unwrap();
        let sum = checksum(&bytes);
        (buffer, callback, sum)
    }
}

/// Drive one full FileChooser dialog to a physical acceptance, from the mock
/// compositor's side. Returns the checksum of the first (640x432) frame, which
/// must equal the checksum the dialog reported to the caller.
fn serve(listener: UnixListener, runtime: PathBuf, keymap_dir: PathBuf) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    let (stream, _) = listener.accept().unwrap();
    let mut peer = Peer::new(stream);

    // The client requests the registry and the initial sync.
    let registry = peer.expect(deadline, 1, 1);
    assert_eq!(Cursor::new(&registry.payload).u32().unwrap(), REGISTRY);
    let sync = peer.expect(deadline, 1, 0);
    assert_eq!(Cursor::new(&sync.payload).u32().unwrap(), SYNC);

    // Advertise the exact eleven globals, then complete the roundtrip.
    for (name, interface, version) in GLOBALS {
        let mut global = Builder::new();
        global.u32(name);
        global.string(interface).unwrap();
        global.u32(version);
        peer.send(REGISTRY, 0, global);
    }
    let mut done = Builder::new();
    done.u32(1);
    peer.send(SYNC, 0, done);

    // Drain the client's binds and surface setup, up to the initial commit
    // after get_dialog, capturing the seat and manager ids and checking the
    // authenticated title and the dialog request.
    let mut seat = None;
    let mut manager = None;
    loop {
        let request = peer.next(deadline);
        match (request.object, request.opcode) {
            (REGISTRY, 0) => {
                let mut args = Cursor::new(&request.payload);
                args.u32().unwrap();
                let interface = args.string().unwrap();
                args.u32().unwrap();
                let id = args.u32().unwrap();
                args.finish().unwrap();
                match interface.as_str() {
                    "wl_seat" => seat = Some(id),
                    "td_portal_manager_v1" => manager = Some(id),
                    _ => {}
                }
            }
            (TOPLEVEL, 2) => {
                let mut args = Cursor::new(&request.payload);
                assert_eq!(args.string().unwrap(), "firefox — Choose a report");
                args.finish().unwrap();
            }
            (object, 0) if Some(object) == manager => {
                let mut args = Cursor::new(&request.payload);
                assert_eq!(args.u32().unwrap(), SURFACE);
                assert_eq!(args.string().unwrap(), PARENT);
                assert_eq!(args.u32().unwrap(), 0);
                args.finish().unwrap();
            }
            (SURFACE, 6) => break,
            // create_surface, get_xdg_surface, get_toplevel, get_data_device.
            _ => {}
        }
    }
    let seat = seat.expect("client bound the seat");
    let manager = manager.expect("client bound the portal manager");

    // Offer XRGB and a keyboard; the client creates the keyboard on the seat.
    let mut format = Builder::new();
    format.u32(XRGB8888);
    peer.send(SHM, 0, format);
    let mut capabilities = Builder::new();
    capabilities.u32(KEYBOARD_CAPABILITY);
    peer.send(seat, 0, capabilities);
    let get_keyboard = peer.expect(deadline, seat, 1);
    let keyboard = Cursor::new(&get_keyboard.payload).u32().unwrap();

    // Hand over the pinned keymap, focus the surface, and snapshot modifiers so
    // presses translate.
    let mut keymap_bytes = XKB_KEYMAP.as_bytes().to_vec();
    keymap_bytes.push(0);
    let keymap = backing_file(&keymap_dir, keymap_bytes.len()).unwrap();
    keymap.write_all_at(&keymap_bytes, 0).unwrap();
    let mut keymap_event = Builder::new();
    keymap_event.u32(1); // xkb_v1
    keymap_event.u32(u32::try_from(keymap_bytes.len()).unwrap());
    peer.send_fd(keyboard, 0, keymap_event, &keymap);
    let mut enter = Builder::new();
    enter.u32(1);
    enter.u32(SURFACE);
    enter.array(&[]).unwrap();
    peer.send(keyboard, 1, enter);
    let mut modifiers = Builder::new();
    for _ in 0..5 {
        modifiers.u32(0);
    }
    peer.send(keyboard, 4, modifiers);

    // Admit the dialog and configure the toplevel; a zero extent keeps the
    // toolkit default (640x432).
    let mut state = Builder::new();
    state.u32(SURFACE);
    state.u32(1);
    peer.send(manager, 0, state);
    configure(&mut peer, 0, 0, 23);
    let ack = peer.expect(deadline, XDG_SURFACE, 4);
    assert_eq!(Cursor::new(&ack.payload).u32().unwrap(), 23);
    let (first_buffer, first_callback, first) = peer.expect_frame(deadline, 640, 432, &runtime);
    release(&mut peer, first_buffer, first_callback, 1000);

    // A resize renders a different frame; the caller's first-frame notice does
    // not move to it. The client reuses the freed slot for the new extent, so
    // it destroys the first, differently sized buffer before making the next.
    configure(&mut peer, 600, 402, 24);
    let ack = peer.expect(deadline, XDG_SURFACE, 4);
    assert_eq!(Cursor::new(&ack.payload).u32().unwrap(), 24);
    peer.expect(deadline, first_buffer, 0);
    let (second_buffer, second_callback, second) = peer.expect_frame(deadline, 600, 402, &runtime);
    assert_ne!(second, first);
    release(&mut peer, second_buffer, second_callback, 1001);

    // A physical Return accepts the single file; the dialog dismisses and the
    // mock confirms the dismissal.
    let mut key = Builder::new();
    key.u32(31);
    key.u32(1);
    key.u32(28);
    key.u32(KEY_PRESSED);
    peer.send(keyboard, 3, key);
    loop {
        let request = peer.next(deadline);
        if (request.object, request.opcode) == (manager, 1) {
            assert_eq!(Cursor::new(&request.payload).u32().unwrap(), SURFACE);
            break;
        }
    }
    let mut dismissed = Builder::new();
    dismissed.u32(SURFACE);
    dismissed.u32(DISMISSED);
    peer.send(manager, 0, dismissed);
    first
}

fn configure(peer: &mut Peer, width: i32, height: i32, serial: u32) {
    let mut toplevel = Builder::new();
    toplevel.i32(width);
    toplevel.i32(height);
    toplevel.array(&[]).unwrap();
    peer.send(TOPLEVEL, 0, toplevel);
    let mut surface = Builder::new();
    surface.u32(serial);
    peer.send(XDG_SURFACE, 0, surface);
}

fn release(peer: &mut Peer, buffer: u32, callback: u32, serial: u32) {
    peer.send(buffer, 0, Builder::new());
    let mut done = Builder::new();
    done.u32(serial);
    peer.send(callback, 0, done);
}

#[test]
fn dialog_presents_pixels_and_accepts_a_physical_return() {
    let temp = Temp::new("round-trip");
    let display = temp.0.join("display");
    let runtime = temp.0.join("runtime");
    let root = temp.0.join("Downloads");
    for directory in [&display, &runtime, &root] {
        fs::create_dir(directory).unwrap();
    }
    fs::write(root.join("report.txt"), b"authenticated fixture").unwrap();
    let socket = display.join("wayland-0");
    let listener = UnixListener::bind(&socket).unwrap();

    let server_runtime = runtime.clone();
    let server_keymap = temp.0.clone();
    let server = thread::spawn(move || serve(listener, server_runtime, server_keymap));

    let (sender, receiver) = mpsc::channel();
    let config = DialogConfig {
        socket,
        runtime_directory: runtime,
        title: "Choose a report".into(),
        parent_handle: PARENT.into(),
        app_id: "firefox".into(),
        host_root: root,
        guest_root: PathBuf::from("/home/td/Downloads"),
        mode: Mode::OpenFile { multiple: false },
        accept_label: None,
        filter: None,
        connector: Arc::new(AtomicBool::new(false)),
    };
    spawn(config, move |notice| {
        sender
            .send(notice)
            .map_err(|error| format!("record dialog notice: {error}"))
    })
    .unwrap();

    let timeout = Duration::from_secs(10);
    assert!(matches!(
        receiver.recv_timeout(timeout).unwrap(),
        Notice::Connected(_)
    ));
    let reported = match receiver.recv_timeout(timeout).unwrap() {
        Notice::Presented {
            width,
            height,
            checksum,
        } => {
            assert_eq!((width, height), (640, 432));
            checksum
        }
        other => panic!("expected a first-frame notice, got {other:?}"),
    };
    match receiver.recv_timeout(timeout).unwrap() {
        Notice::Completed(Ok(Outcome::Accepted(uris))) => {
            assert_eq!(uris, vec!["file:///home/td/Downloads/report.txt".to_string()]);
        }
        other => panic!("expected an accepted completion, got {other:?}"),
    }

    let rendered = server.join().unwrap();
    assert_eq!(reported, rendered);
}
