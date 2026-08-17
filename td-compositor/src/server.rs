use crate::configure::{Configure, ConfigureTracker, ToplevelState, ViewStatus};
use crate::keyboard::{KeyboardEvent, KeyboardSnapshot, XKB_KEYMAP};
use crate::layout::ViewLayout;
use crate::pointer::{PointerEvent, PointerSnapshot, RoutedPointerFrame};
use crate::runtime::{KeyboardDelivery, KeyboardSubscriptionStop, Runtime, SubscriptionStop};
use crate::scene::{
    CursorRequest, InputRegion, SharedInputRegion, Surface, SurfaceKey, MAX_CURSOR_DIMENSION,
    MAX_INPUT_REGION_OPERATIONS, SHM_ARGB8888, SHM_XRGB8888,
};
#[cfg(test)]
use crate::scene::{GAP, TITLE_HEIGHT};
use crate::{socket, sys, wire, MAX_UI_DIMENSION, MAX_UI_FRAME_BYTES};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::Write;
use std::net::Shutdown;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;

const GLOBAL_COMPOSITOR: u32 = 1;
const GLOBAL_SHM: u32 = 2;
const GLOBAL_OUTPUT: u32 = 3;
const GLOBAL_XDG_WM_BASE: u32 = 4;
const GLOBAL_SEAT: u32 = 5;
const GLOBAL_DECORATION: u32 = 6;
const WL_DISPLAY_ERROR_IMPLEMENTATION: u32 = 3;
const WL_SEAT_ERROR_MISSING_CAPABILITY: u32 = 0;
const WL_POINTER_ERROR_ROLE: u32 = 0;
/// `wl_pointer.axis_source`'s `wheel`. The other three sources — finger,
/// continuous, wheel tilt — describe devices this compositor does not read,
/// and naming one it cannot tell apart would be worse than the silence a
/// version-4 client gets: a client uses the source to decide whether to
/// kinetic-scroll, and a wheel that claimed to be a finger would coast.
const WL_POINTER_AXIS_SOURCE_WHEEL: u32 = 0;
/// `axis_source` and `axis_discrete`, both since version 5. Sent WITH the
/// axis event rather than instead of it: `axis` is the one every version
/// understands, and the other two only qualify it.
const WL_POINTER_AXIS_SOURCE: u16 = 6;
const WL_POINTER_AXIS_DISCRETE: u16 = 8;
const WL_POINTER_AXIS_EVENTS_SINCE: u32 = 5;
/// `axis_discrete` is DEPRECATED at version 8: the protocol says it is not
/// sent to a client supporting 8 or later, which gets `axis_value120` instead.
/// Unreachable while `wl_seat` is advertised at 7 — and that is exactly why it
/// is pinned rather than left as a `>= 5`. A version bump would otherwise
/// start sending a version-8 client an event the protocol forbids it, with
/// nothing in the tree saying the two numbers were related;
/// `the_seat_version_stays_inside_what_axis_discrete_may_be_sent_to` is what
/// makes that bump red instead.
const WL_POINTER_AXIS_DISCRETE_LAST: u32 = 7;
/// The highest `wl_seat` this advertises, and so the highest `wl_pointer` a
/// client can bind — the two are one number in this protocol. Named because
/// `WL_POINTER_AXIS_DISCRETE_LAST` is a claim ABOUT it: a bare 7 in three
/// places is three places to change and nothing relating them.
const SEAT_VERSION: u32 = 7;
/// Checked where it CANNOT be skipped. Both numbers are constants, so this is
/// a compile-time relation and a test asserting it would only ever be true —
/// clippy says as much. Raising `SEAT_VERSION` past the deprecation now fails
/// the build rather than quietly sending a version-8 client an event the
/// protocol forbids it.
const _: () = assert!(SEAT_VERSION <= WL_POINTER_AXIS_DISCRETE_LAST);
/// `zxdg_toplevel_decoration_v1.mode`. td answers `server_side` and never the
/// other one: a tile already carries a title band the compositor draws, so a
/// client drawing its own would be a second title over the same window, inside
/// the geometry the layout gave it.
const DECORATION_MODE_SERVER_SIDE: u32 = 2;
/// The client's own spelling of the mode it would prefer. Read so the request
/// is validated on the wire and so a `set_mode(server_side)` is not mistaken
/// for a client asking for something td refuses; the ANSWER does not depend on
/// it, which is what the protocol allows and what tiling requires.
const DECORATION_MODE_CLIENT_SIDE: u32 = 1;
/// `zxdg_toplevel_decoration_v1.error`. Every one is a CLIENT mistake — none
/// reports anything about the compositor — so each is raised against the
/// object that made it rather than turned into a disconnect with no code.
const DECORATION_ERROR_UNCONFIGURED_BUFFER: u32 = 0;
const DECORATION_ERROR_ALREADY_CONSTRUCTED: u32 = 1;
const DECORATION_ERROR_ORPHANED: u32 = 2;
/// `configure`, the interface's only event.
const DECORATION_CONFIGURE: u16 = 0;
const MAX_POOL_BYTES: usize = 64 * 1024 * 1024;
const MAX_CLIENT_BUFFER: usize = 256 * 1024;
const MAX_PENDING_FDS: usize = 64;
const MAX_OBJECTS: usize = 512;
const MAX_CLIENTS: usize = 32;
const MAX_CLIENT_INPUT_REGION_OPERATIONS: usize = 4_096;

static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);
static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);
static NEXT_BUFFER_SERIAL: AtomicU64 = AtomicU64::new(1);
static NEXT_KEYMAP_FILE: AtomicU64 = AtomicU64::new(1);
static ACTIVE_CLIENTS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
struct Pool {
    file: Arc<File>,
    size: usize,
}

#[derive(Clone)]
struct Buffer {
    serial: u64,
    file: Arc<File>,
    offset: usize,
    width: usize,
    height: usize,
    stride: usize,
    format: u32,
}

#[derive(Clone)]
enum PendingBuffer {
    Detach,
    Buffer { object: u32, buffer: Buffer },
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SurfaceRole {
    Xdg(u32),
    Cursor,
}

#[derive(Clone, Default)]
struct SurfaceState {
    pending_buffer: Option<PendingBuffer>,
    pending_input_region: Option<Option<SharedInputRegion>>,
    input_region: Option<SharedInputRegion>,
    frame_callbacks: Vec<u32>,
    role: Option<SurfaceRole>,
}

#[derive(Clone)]
enum Object {
    Display,
    Registry,
    Compositor,
    Region(SharedInputRegion),
    Shm,
    Pool(Pool),
    Buffer(Buffer),
    Surface(SurfaceState),
    Callback,
    Output {
        version: u32,
    },
    Seat {
        version: u32,
    },
    Keyboard {
        version: u32,
    },
    Pointer {
        version: u32,
    },
    XdgWmBase,
    XdgSurface {
        surface: u32,
        toplevel: Option<u32>,
        configure: Arc<Mutex<ConfigureTracker>>,
    },
    XdgToplevel {
        xdg_surface: u32,
        /// The decoration object made for this toplevel, if one has been. Held
        /// here rather than in a map beside the objects for `XdgSurface`'s
        /// reason: the pairing is the toplevel's own state, and a second place
        /// to record it is a second place to leave it stale. It is what makes
        /// a repeat `get_toplevel_decoration` `already_constructed` and a
        /// toplevel destroyed under a live decoration `orphaned`.
        decoration: Option<u32>,
    },
    DecorationManager,
    ToplevelDecoration {
        toplevel: u32,
    },
}

#[derive(Clone)]
struct ConfigureRegistration {
    xdg_surface: u32,
    toplevel: u32,
    tracker: Arc<Mutex<ConfigureTracker>>,
}

#[derive(Clone, Copy)]
struct KeyboardRegistration {
    after_revision: u64,
}

#[derive(Clone, Copy)]
struct PointerRegistration {
    after_revision: u64,
    version: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PointerEnterAuthority {
    serial: u32,
    surface: u32,
    after_revision: u64,
}

type DeleteReservation = Arc<Mutex<()>>;
type PendingDeletes = Arc<Mutex<BTreeMap<u32, DeleteReservation>>>;
type PointerAuthority = Arc<Mutex<Option<PointerEnterAuthority>>>;

#[derive(Clone)]
struct KeymapFile {
    file: Arc<File>,
    size: u32,
}

struct Outbound {
    stream: UnixStream,
    disconnected: bool,
}

impl Outbound {
    fn send(&mut self, message: &[u8]) -> Result<(), String> {
        if self.disconnected {
            return Ok(());
        }
        match self.stream.write_all(message) {
            Ok(()) => Ok(()),
            Err(error) if sys::write_peer_disconnected(&error) => {
                self.disconnected = true;
                Ok(())
            }
            Err(error) => Err(format!("write Wayland event: {error}")),
        }
    }

    fn send_with_fd(&mut self, message: &[u8], fd: RawFd) -> Result<(), String> {
        if self.disconnected {
            return Ok(());
        }
        match sys::send_with_fd(&self.stream, message, fd) {
            Ok(()) => Ok(()),
            Err(error) if sys::write_peer_disconnected(&error) => {
                self.disconnected = true;
                Ok(())
            }
            Err(error) => Err(format!("send Wayland descriptor event: {error}")),
        }
    }

    fn disconnect(&mut self) {
        self.disconnected = true;
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

struct Client {
    id: u64,
    stream: UnixStream,
    outbound: Arc<Mutex<Outbound>>,
    configurations: Arc<Mutex<BTreeMap<SurfaceKey, ConfigureRegistration>>>,
    keyboards: Arc<Mutex<BTreeMap<u32, KeyboardRegistration>>>,
    pointers: Arc<Mutex<BTreeMap<u32, PointerRegistration>>>,
    pointer_authority: PointerAuthority,
    keyboard_active: Arc<AtomicBool>,
    pointer_active: Arc<AtomicBool>,
    pending_deletes: PendingDeletes,
    objects: BTreeMap<u32, Object>,
    runtime: Arc<Mutex<Runtime>>,
    keymap: KeymapFile,
    protocol_error_code: u32,
    /// The object an error names, when that is not the one whose request
    /// raised it. Wayland scopes an error CODE to the INTERFACE of the object
    /// it is reported against, so a code borrowed from another interface is
    /// read as whatever that number means on this one — `orphaned` (2) against
    /// the xdg_toplevel whose destroy raised it is `xdg_toplevel.invalid_size`,
    /// a different fault on a different object. Cleared per dispatch beside the
    /// code, so it can only ever describe the request that set it.
    protocol_error_object: Option<u32>,
    mapped_bytes: BTreeMap<u32, usize>,
    mapped_total: usize,
}

struct ClientPermit;

impl ClientPermit {
    fn acquire() -> Result<ClientPermit, String> {
        let mut current = ACTIVE_CLIENTS.load(Ordering::Acquire);
        loop {
            if current >= MAX_CLIENTS {
                return Err(format!(
                    "refusing Wayland client: {current} connections already active"
                ));
            }
            match ACTIVE_CLIENTS.compare_exchange_weak(
                current,
                current.saturating_add(1),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(ClientPermit),
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for ClientPermit {
    fn drop(&mut self) {
        ACTIVE_CLIENTS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn next_serial() -> u32 {
    let value = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
    let folded = value % u64::from(u32::MAX);
    u32::try_from(folded).unwrap_or(1).max(1)
}

fn configure_state_bytes(state: ToplevelState) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8);
    if state.fullscreen {
        bytes.extend_from_slice(&2u32.to_ne_bytes());
    }
    if state.activated {
        bytes.extend_from_slice(&4u32.to_ne_bytes());
    }
    bytes
}

fn send_configure(
    outbound: &Arc<Mutex<Outbound>>,
    registration: &ConfigureRegistration,
    configure: Configure,
) -> Result<(), String> {
    let states = configure_state_bytes(configure.state);
    let mut toplevel = wire::Builder::new();
    toplevel.i32(configure.state.width);
    toplevel.i32(configure.state.height);
    toplevel.array(&states)?;
    let toplevel = toplevel.message(registration.toplevel, 0)?;

    let mut surface = wire::Builder::new();
    surface.u32(configure.serial);
    let surface = surface.message(registration.xdg_surface, 0)?;

    let mut outbound = outbound
        .lock()
        .map_err(|_| "Wayland outbound lock poisoned".to_string())?;
    outbound.send(&toplevel)?;
    outbound.send(&surface)
}

fn send_initial_configure(
    outbound: &Arc<Mutex<Outbound>>,
    registration: &ConfigureRegistration,
) -> Result<(), String> {
    let mut tracker = registration
        .tracker
        .lock()
        .map_err(|_| "XDG configure tracker lock poisoned".to_string())?;
    let configure = tracker.initial(next_serial())?;
    let sent = send_configure(outbound, registration, configure);
    drop(tracker);
    sent
}

fn view_status(
    snapshot: &BTreeMap<SurfaceKey, ViewLayout>,
    key: SurfaceKey,
) -> Result<ViewStatus, String> {
    let Some(view) = snapshot.get(&key) else {
        return Ok(ViewStatus::Unmapped);
    };
    let width = i32::try_from(view.rect.width.max(1))
        .map_err(|_| "tile width exceeds XDG i32".to_string())?;
    let height = i32::try_from(view.rect.height.max(1))
        .map_err(|_| "tile height exceeds XDG i32".to_string())?;
    let state = ToplevelState {
        width,
        height,
        activated: view.activated,
        fullscreen: view.fullscreen,
    };
    Ok(if view.visible {
        ViewStatus::Visible(state)
    } else {
        ViewStatus::Hidden(state)
    })
}

fn update_configure(
    outbound: &Arc<Mutex<Outbound>>,
    registration: &ConfigureRegistration,
    status: ViewStatus,
) -> Result<(), String> {
    let mut tracker = registration
        .tracker
        .lock()
        .map_err(|_| "XDG configure tracker lock poisoned".to_string())?;
    let configure = tracker.update(status, next_serial())?;
    if let Some(configure) = configure {
        let sent = send_configure(outbound, registration, configure);
        drop(tracker);
        sent?;
    }
    Ok(())
}

fn configure_worker(
    receiver: Receiver<()>,
    stop: SubscriptionStop,
    runtime: Arc<Mutex<Runtime>>,
    configurations: Arc<Mutex<BTreeMap<SurfaceKey, ConfigureRegistration>>>,
    outbound: Arc<Mutex<Outbound>>,
) -> Result<(), String> {
    while receiver.recv().is_ok() {
        if stop.is_stopped() {
            return Ok(());
        }
        let snapshot = runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .layout_snapshot();
        let configurations = configurations
            .lock()
            .map_err(|_| "configure registration lock poisoned".to_string())?;
        for (key, registration) in configurations.iter() {
            update_configure(&outbound, registration, view_status(&snapshot, *key)?)?;
        }
        if outbound
            .lock()
            .map_err(|_| "Wayland outbound lock poisoned".to_string())?
            .disconnected
        {
            return Ok(());
        }
    }
    Ok(())
}

fn keymap_file(directory: &Path) -> Result<KeymapFile, String> {
    let mut bytes = Vec::with_capacity(XKB_KEYMAP.len().saturating_add(1));
    bytes.extend_from_slice(XKB_KEYMAP.as_bytes());
    bytes.push(0);
    let size = u32::try_from(bytes.len()).map_err(|_| "XKB keymap exceeds u32".to_string())?;
    for _ in 0..32 {
        let sequence = NEXT_KEYMAP_FILE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(".td-keymap-{}-{sequence}", std::process::id()));
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create XKB keymap {}: {error}", path.display())),
        };
        let read_only = (|| {
            // Restore owner access that an unusual umask may remove.
            file.set_permissions(Permissions::from_mode(0o600))
                .map_err(|e| format!("chmod XKB keymap {}: {e}", path.display()))?;
            file.write_all_at(&bytes, 0)
                .map_err(|e| format!("write XKB keymap: {e}"))?;
            File::open(format!("/proc/self/fd/{}", file.as_raw_fd()))
                .map_err(|e| format!("reopen XKB keymap read-only: {e}"))
        })();
        let unlink = fs::remove_file(&path)
            .map_err(|e| format!("unlink XKB keymap {}: {e}", path.display()));
        return match (read_only, unlink) {
            (Ok(read_only), Ok(())) => Ok(KeymapFile {
                file: Arc::new(read_only),
                size,
            }),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(unlink_error)) => Err(format!("{error}; {unlink_error}")),
        };
    }
    Err("could not reserve a unique XKB keymap file".into())
}

fn keyboard_message(object: u32, serial: u32, event: &KeyboardEvent) -> Result<Vec<u8>, String> {
    let (opcode, builder) = match event {
        KeyboardEvent::Enter { surface, keys } => {
            let mut bytes = Vec::with_capacity(keys.len().saturating_mul(4));
            for key in keys {
                bytes.extend_from_slice(&key.to_ne_bytes());
            }
            let mut builder = wire::Builder::new();
            builder.u32(serial);
            builder.u32(surface.object);
            builder.array(&bytes)?;
            (1, builder)
        }
        KeyboardEvent::Leave { surface } => {
            let mut builder = wire::Builder::new();
            builder.u32(serial);
            builder.u32(surface.object);
            (2, builder)
        }
        KeyboardEvent::Key { input, .. } => {
            let mut builder = wire::Builder::new();
            builder.u32(serial);
            builder.u32(input.time);
            builder.u32(input.key);
            builder.u32(input.state.wire());
            (3, builder)
        }
        KeyboardEvent::Modifiers { state, .. } => {
            let mut builder = wire::Builder::new();
            builder.u32(serial);
            builder.u32(state.depressed);
            builder.u32(state.latched);
            builder.u32(state.locked);
            builder.u32(state.group);
            (4, builder)
        }
    };
    builder.message(object, opcode)
}

fn send_keyboard_event(
    outbound: &Arc<Mutex<Outbound>>,
    object: u32,
    serial: u32,
    event: &KeyboardEvent,
) -> Result<(), String> {
    let message = keyboard_message(object, serial, event)?;
    outbound
        .lock()
        .map_err(|_| "Wayland outbound lock poisoned".to_string())?
        .send(&message)
}

fn send_keyboard_initial(
    outbound: &Arc<Mutex<Outbound>>,
    object: u32,
    version: u32,
    client: u64,
    snapshot: &KeyboardSnapshot,
    file: &File,
    size: u32,
) -> Result<(), String> {
    let mut keymap = wire::Builder::new();
    keymap.u32(1);
    keymap.u32(size);
    let keymap = keymap.message(object, 0)?;
    let mut outbound = outbound
        .lock()
        .map_err(|_| "Wayland outbound lock poisoned".to_string())?;
    outbound.send_with_fd(&keymap, file.as_raw_fd())?;
    if version >= 4 {
        let mut repeat = wire::Builder::new();
        repeat.i32(25);
        repeat.i32(600);
        outbound.send(&repeat.message(object, 5)?)?;
    }
    if let Some(surface) = snapshot.focus.filter(|surface| surface.client == client) {
        outbound.send(&keyboard_message(
            object,
            next_serial(),
            &KeyboardEvent::Enter {
                surface,
                keys: snapshot.keys.clone(),
            },
        )?)?;
        outbound.send(&keyboard_message(
            object,
            next_serial(),
            &KeyboardEvent::Modifiers {
                surface,
                state: snapshot.modifiers,
            },
        )?)?;
    }
    Ok(())
}

fn pointer_fixed(value: i32) -> Result<i32, String> {
    value
        .checked_mul(256)
        .ok_or_else(|| format!("pointer coordinate {value} exceeds wl_fixed"))
}

fn pointer_message(
    object: u32,
    serial: Option<u32>,
    event: &PointerEvent,
) -> Result<Vec<u8>, String> {
    let (opcode, builder) = match event {
        PointerEvent::Enter { target } => {
            let mut builder = wire::Builder::new();
            builder.u32(serial.ok_or_else(|| "pointer enter lacks serial".to_string())?);
            builder.u32(target.surface.object);
            builder.i32(pointer_fixed(target.x)?);
            builder.i32(pointer_fixed(target.y)?);
            (0, builder)
        }
        PointerEvent::Leave { surface } => {
            let mut builder = wire::Builder::new();
            builder.u32(serial.ok_or_else(|| "pointer leave lacks serial".to_string())?);
            builder.u32(surface.object);
            (1, builder)
        }
        PointerEvent::Motion { time, target } => {
            let mut builder = wire::Builder::new();
            builder.u32(*time);
            builder.i32(pointer_fixed(target.x)?);
            builder.i32(pointer_fixed(target.y)?);
            (2, builder)
        }
        PointerEvent::Button { input, .. } => {
            let mut builder = wire::Builder::new();
            builder.u32(serial.ok_or_else(|| "pointer button lacks serial".to_string())?);
            builder.u32(input.time);
            builder.u32(input.button);
            builder.u32(input.state.wire());
            (3, builder)
        }
        PointerEvent::Axis { time, step, .. } => {
            let mut builder = wire::Builder::new();
            builder.u32(*time);
            builder.u32(step.axis.wire());
            builder.i32(pointer_fixed(step.value)?);
            (4, builder)
        }
    };
    builder.message(object, opcode)
}

/// The messages one routed event becomes for a client at `version`. Only an
/// axis is ever more than one, and only from version 5: the notch count is a
/// separate event qualifying the axis it accompanies, and it precedes it
/// because a client reading the frame in order should know what kind of
/// scroll it is before it is handed a distance.
///
/// `carries_source` is the frame's answer rather than this event's. The
/// protocol says `axis_source` "carries the source information for all events
/// within that frame", so it belongs to the FRAME — and a tilting wheel, which
/// reports both axes at once, is two axis events under one frame. Passed in
/// rather than decided here so this stays a function of one event.
fn pointer_messages(
    object: u32,
    version: u32,
    serial: Option<u32>,
    event: &PointerEvent,
    carries_source: bool,
) -> Result<Vec<Vec<u8>>, String> {
    let mut messages = Vec::new();
    if let PointerEvent::Axis { step, .. } = event {
        if version >= WL_POINTER_AXIS_EVENTS_SINCE {
            if carries_source {
                let mut source = wire::Builder::new();
                source.u32(WL_POINTER_AXIS_SOURCE_WHEEL);
                messages.push(source.message(object, WL_POINTER_AXIS_SOURCE)?);
            }
            if version <= WL_POINTER_AXIS_DISCRETE_LAST {
                let mut discrete = wire::Builder::new();
                discrete.u32(step.axis.wire());
                discrete.i32(step.detents);
                messages.push(discrete.message(object, WL_POINTER_AXIS_DISCRETE)?);
            }
        }
    }
    messages.push(pointer_message(object, serial, event)?);
    Ok(messages)
}

fn pointer_event_serial(
    event: &PointerEvent,
    authority: Option<PointerEnterAuthority>,
) -> Option<u32> {
    match event {
        PointerEvent::Enter { target } => Some(
            authority
                .filter(|candidate| candidate.surface == target.surface.object)
                .map_or_else(next_serial, |candidate| candidate.serial),
        ),
        PointerEvent::Leave { .. } | PointerEvent::Button { .. } => Some(next_serial()),
        // Neither carries one: the protocol gives a serial to the events a
        // client may quote back as authority for a request — a grab, a
        // cursor set — and neither a move nor a notch is one of those.
        PointerEvent::Motion { .. } | PointerEvent::Axis { .. } => None,
    }
}

fn send_pointer_initial(
    outbound: &Arc<Mutex<Outbound>>,
    object: u32,
    version: u32,
    client: u64,
    snapshot: PointerSnapshot,
    serial: Option<u32>,
) -> Result<(), String> {
    let Some(target) = snapshot
        .focus
        .filter(|target| target.surface.client == client)
    else {
        return Ok(());
    };
    let serial = serial.ok_or_else(|| "pointer enter lacks an authority serial".to_string())?;
    let enter = pointer_message(object, Some(serial), &PointerEvent::Enter { target })?;
    let mut outbound = outbound
        .lock()
        .map_err(|_| "Wayland outbound lock poisoned".to_string())?;
    outbound.send(&enter)?;
    if version >= 5 {
        outbound.send(&wire::Builder::new().message(object, 5)?)?;
    }
    Ok(())
}

fn send_pointer_frame(
    outbound: &Arc<Mutex<Outbound>>,
    pointers: &BTreeMap<u32, PointerRegistration>,
    authority: &mut Option<PointerEnterAuthority>,
    frame: &RoutedPointerFrame,
) -> Result<(), String> {
    let serials: Vec<Option<u32>> = frame
        .events
        .iter()
        .map(|event| pointer_event_serial(event, *authority))
        .collect();
    // One per FRAME, so the first axis carries the source and a second — a
    // tilting wheel reports both — carries none. Computed once here rather
    // than per pointer object, because which axis is first is a property of
    // the frame and not of who is being sent it.
    let mut seen_axis = false;
    let sources: Vec<bool> = frame
        .events
        .iter()
        .map(|event| {
            let axis = matches!(event, PointerEvent::Axis { .. });
            let first = axis && !seen_axis;
            seen_axis |= axis;
            first
        })
        .collect();
    let mut sent = false;
    let mut outbound = outbound
        .lock()
        .map_err(|_| "Wayland outbound lock poisoned".to_string())?;
    for (object, registration) in pointers {
        if frame.revision <= registration.after_revision {
            continue;
        }
        sent = true;
        for ((event, serial), source) in frame.events.iter().zip(&serials).zip(&sources) {
            for message in pointer_messages(*object, registration.version, *serial, event, *source)?
            {
                outbound.send(&message)?;
            }
        }
        if registration.version >= 5 {
            outbound.send(&wire::Builder::new().message(*object, 5)?)?;
        }
    }
    if sent
        && authority
            .as_ref()
            .is_none_or(|current| frame.revision > current.after_revision)
    {
        for (event, serial) in frame.events.iter().zip(&serials) {
            match event {
                PointerEvent::Enter { target } => {
                    let serial =
                        serial.ok_or_else(|| "pointer enter lacks generated serial".to_string())?;
                    *authority = Some(PointerEnterAuthority {
                        serial,
                        surface: target.surface.object,
                        after_revision: frame.revision,
                    });
                }
                PointerEvent::Leave { .. } => *authority = None,
                _ => {}
            }
        }
    }
    Ok(())
}

fn send_reserved_delete_id(
    outbound: &Arc<Mutex<Outbound>>,
    pending_deletes: &PendingDeletes,
    id: u32,
) -> Result<(), String> {
    let reservation = pending_deletes
        .lock()
        .map_err(|_| "pending object deletion lock poisoned".to_string())?
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("object {id} has no pending deletion"))?;
    let _reservation = reservation
        .lock()
        .map_err(|_| format!("object {id} deletion lock poisoned"))?;
    let mut event = wire::Builder::new();
    event.u32(id);
    let message = event.message(1, 1)?;
    outbound
        .lock()
        .map_err(|_| "Wayland outbound lock poisoned".to_string())?
        .send(&message)?;
    let removed = pending_deletes
        .lock()
        .map_err(|_| "pending object deletion lock poisoned".to_string())?
        .remove(&id);
    if removed.is_some() {
        Ok(())
    } else {
        Err(format!("object {id} deletion reservation disappeared"))
    }
}

fn seat_worker(
    receiver: Receiver<KeyboardDelivery>,
    stop: KeyboardSubscriptionStop,
    keyboards: Arc<Mutex<BTreeMap<u32, KeyboardRegistration>>>,
    pointers: Arc<Mutex<BTreeMap<u32, PointerRegistration>>>,
    pointer_authority: PointerAuthority,
    pending_deletes: PendingDeletes,
    outbound: Arc<Mutex<Outbound>>,
) -> Result<(), String> {
    while let Ok(delivery) = receiver.recv() {
        if stop.is_stopped() {
            let mut pending = pending_deletes
                .lock()
                .map_err(|_| "pending object deletion lock poisoned".to_string())?;
            for delivery in std::iter::once(delivery).chain(receiver.try_iter()) {
                if let KeyboardDelivery::DeleteId(id) = delivery {
                    pending.remove(&id);
                }
            }
            return Ok(());
        }
        let event = match delivery {
            KeyboardDelivery::Event(event) => Some(event),
            KeyboardDelivery::Pointer(frame) => {
                let pointers = pointers
                    .lock()
                    .map_err(|_| "pointer registration lock poisoned".to_string())?;
                let mut authority = pointer_authority
                    .lock()
                    .map_err(|_| "pointer authority lock poisoned".to_string())?;
                send_pointer_frame(&outbound, &pointers, &mut authority, &frame)?;
                if outbound
                    .lock()
                    .map_err(|_| "Wayland outbound lock poisoned".to_string())?
                    .disconnected
                {
                    return Ok(());
                }
                None
            }
            KeyboardDelivery::DeleteId(id) => {
                send_reserved_delete_id(&outbound, &pending_deletes, id)?;
                if outbound
                    .lock()
                    .map_err(|_| "Wayland outbound lock poisoned".to_string())?
                    .disconnected
                {
                    return Ok(());
                }
                continue;
            }
        };
        let Some(event) = event else {
            continue;
        };
        let keyboards = keyboards
            .lock()
            .map_err(|_| "keyboard registration lock poisoned".to_string())?;
        let serial = next_serial();
        for (object, registration) in keyboards.iter() {
            if event.revision > registration.after_revision {
                send_keyboard_event(&outbound, *object, serial, &event.event)?;
            }
        }
        if outbound
            .lock()
            .map_err(|_| "Wayland outbound lock poisoned".to_string())?
            .disconnected
        {
            return Ok(());
        }
    }
    if stop.is_stopped() {
        Ok(())
    } else {
        Err("seat event queue closed before client teardown".into())
    }
}

#[allow(clippy::too_many_arguments)]
fn supervise_seat_worker(
    client: u64,
    receiver: Receiver<KeyboardDelivery>,
    stop: KeyboardSubscriptionStop,
    keyboards: Arc<Mutex<BTreeMap<u32, KeyboardRegistration>>>,
    pointers: Arc<Mutex<BTreeMap<u32, PointerRegistration>>>,
    pointer_authority: PointerAuthority,
    pending_deletes: PendingDeletes,
    outbound: Arc<Mutex<Outbound>>,
) -> Result<(), String> {
    let result = seat_worker(
        receiver,
        stop,
        keyboards,
        pointers,
        pointer_authority,
        pending_deletes,
        Arc::clone(&outbound),
    );
    if let Err(error) = &result {
        eprintln!("td-compositor: client {client} seat: {error}");
        if let Ok(mut outbound) = outbound.lock() {
            outbound.disconnect();
        }
    }
    result
}

fn client_surface_total(current: usize, prior: usize, proposed: usize) -> Result<usize, String> {
    let retained = current
        .checked_sub(prior)
        .ok_or_else(|| "client surface byte accounting underflow".to_string())?;
    let next = retained
        .checked_add(proposed)
        .ok_or_else(|| "client surface byte accounting overflow".to_string())?;
    if next > MAX_UI_FRAME_BYTES {
        return Err(format!(
            "client surfaces need {next} bytes, exceeding {MAX_UI_FRAME_BYTES}"
        ));
    }
    Ok(next)
}

#[cfg(test)]
fn request(object: u32, opcode: u16, builder: wire::Builder) -> Result<wire::Message, String> {
    let mut encoded = builder.message(object, opcode)?;
    wire::take(&mut encoded)?.ok_or_else(|| "request builder emitted no message".to_string())
}

impl Client {
    fn new(
        id: u64,
        stream: UnixStream,
        runtime: Arc<Mutex<Runtime>>,
        keymap: KeymapFile,
    ) -> Result<Client, String> {
        let mut objects = BTreeMap::new();
        objects.insert(1, Object::Display);
        let writer = stream
            .try_clone()
            .map_err(|e| format!("clone Wayland client stream: {e}"))?;
        Ok(Client {
            id,
            stream,
            outbound: Arc::new(Mutex::new(Outbound {
                stream: writer,
                disconnected: false,
            })),
            configurations: Arc::new(Mutex::new(BTreeMap::new())),
            keyboards: Arc::new(Mutex::new(BTreeMap::new())),
            pointers: Arc::new(Mutex::new(BTreeMap::new())),
            pointer_authority: Arc::new(Mutex::new(None)),
            keyboard_active: Arc::new(AtomicBool::new(false)),
            pointer_active: Arc::new(AtomicBool::new(false)),
            pending_deletes: Arc::new(Mutex::new(BTreeMap::new())),
            objects,
            runtime,
            keymap,
            protocol_error_code: WL_DISPLAY_ERROR_IMPLEMENTATION,
            protocol_error_object: None,
            mapped_bytes: BTreeMap::new(),
            mapped_total: 0,
        })
    }

    fn clear_surface_bytes(&mut self, surface: u32) {
        if let Some(bytes) = self.mapped_bytes.remove(&surface) {
            self.mapped_total = self.mapped_total.saturating_sub(bytes);
        }
    }

    fn unmap_surface(&mut self, surface: u32) -> Result<(), String> {
        self.clear_surface_bytes(surface);
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .unmap(SurfaceKey {
                client: self.id,
                object: surface,
            })
    }

    fn remove_surface(&mut self, surface: u32) -> Result<(), String> {
        self.clear_surface_bytes(surface);
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .remove(SurfaceKey {
                client: self.id,
                object: surface,
            })
    }

    fn send(&mut self, object: u32, opcode: u16, builder: wire::Builder) -> Result<(), String> {
        let message = builder.message(object, opcode)?;
        self.outbound
            .lock()
            .map_err(|_| "Wayland outbound lock poisoned".to_string())?
            .send(&message)
    }

    fn create_keyboard(&mut self, id: u32, version: u32) -> Result<(), String> {
        self.insert(id, Object::Keyboard { version })?;
        let mut keyboards = self
            .keyboards
            .lock()
            .map_err(|_| "keyboard registration lock poisoned".to_string())?;
        self.keyboard_active.store(true, Ordering::Release);
        let snapshot = self
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .keyboard_snapshot();
        // This guard orders initial state against worker events for this object.
        if keyboards
            .insert(
                id,
                KeyboardRegistration {
                    after_revision: snapshot.revision,
                },
            )
            .is_some()
        {
            return Err(format!("keyboard object {id} was already registered"));
        }
        send_keyboard_initial(
            &self.outbound,
            id,
            version,
            self.id,
            &snapshot,
            &self.keymap.file,
            self.keymap.size,
        )
    }

    fn remove_keyboard(&mut self, id: u32) -> Result<(), String> {
        let mut keyboards = self
            .keyboards
            .lock()
            .map_err(|_| "keyboard registration lock poisoned".to_string())?;
        keyboards.remove(&id);
        if keyboards.is_empty() {
            self.keyboard_active.store(false, Ordering::Release);
        }
        drop(keyboards);
        self.remove_object(id)
    }

    fn create_pointer(&mut self, id: u32, version: u32) -> Result<(), String> {
        self.insert(id, Object::Pointer { version })?;
        let mut pointers = self
            .pointers
            .lock()
            .map_err(|_| "pointer registration lock poisoned".to_string())?;
        self.pointer_active.store(true, Ordering::Release);
        let snapshot = self
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .pointer_snapshot();
        let target = snapshot
            .focus
            .filter(|target| target.surface.client == self.id);
        let mut authority = self
            .pointer_authority
            .lock()
            .map_err(|_| "pointer authority lock poisoned".to_string())?;
        let serial = target.map(|target| {
            authority
                .filter(|candidate| candidate.surface == target.surface.object)
                .map_or_else(next_serial, |candidate| candidate.serial)
        });
        let registration = PointerRegistration {
            after_revision: snapshot.revision,
            version,
        };
        send_pointer_initial(&self.outbound, id, version, self.id, snapshot, serial)?;
        if let (Some(target), Some(serial)) = (target, serial) {
            *authority = Some(PointerEnterAuthority {
                serial,
                surface: target.surface.object,
                after_revision: snapshot.revision,
            });
        }
        if pointers.insert(id, registration).is_some() {
            return Err(format!("pointer object {id} was already registered"));
        }
        Ok(())
    }

    fn remove_pointer(&mut self, id: u32) -> Result<(), String> {
        let mut pointers = self
            .pointers
            .lock()
            .map_err(|_| "pointer registration lock poisoned".to_string())?;
        pointers.remove(&id);
        if pointers.is_empty() {
            self.pointer_active.store(false, Ordering::Release);
        }
        drop(pointers);
        self.remove_object(id)
    }

    fn disconnected(&self) -> Result<bool, String> {
        self.outbound
            .lock()
            .map(|outbound| outbound.disconnected)
            .map_err(|_| "Wayland outbound lock poisoned".to_string())
    }

    fn unregister_surface(&mut self, surface: u32) -> Result<(), String> {
        self.configurations
            .lock()
            .map_err(|_| "configure registration lock poisoned".to_string())?
            .remove(&SurfaceKey {
                client: self.id,
                object: surface,
            });
        Ok(())
    }

    fn delete_id(&mut self, id: u32) -> Result<(), String> {
        let mut event = wire::Builder::new();
        event.u32(id);
        self.send(1, 1, event)
    }

    fn remove_surface_object(&mut self, id: u32) -> Result<(), String> {
        if id <= 1 {
            return Err(format!("refusing to delete reserved object {id}"));
        }
        if self.objects.remove(&id).is_none() {
            return Err(format!("object {id} does not exist"));
        }
        let mut pending = self
            .pending_deletes
            .lock()
            .map_err(|_| "pending object deletion lock poisoned".to_string())?;
        if pending.contains_key(&id) {
            return Err(format!("object {id} already has a pending deletion"));
        }
        pending.insert(id, Arc::new(Mutex::new(())));
        drop(pending);
        let queued = self
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .queue_keyboard_delete(self.id, id)?;
        if queued {
            Ok(())
        } else {
            send_reserved_delete_id(&self.outbound, &self.pending_deletes, id)
        }
    }

    fn protocol_error(&mut self, object: u32, code: u32, error: &str) {
        let mut event = wire::Builder::new();
        event.u32(object);
        event.u32(code);
        if event.string(error).is_ok() {
            let _ = self.send(1, 0, event);
        }
    }

    fn fail_protocol(&mut self, code: u32, error: &str) -> Result<(), String> {
        self.protocol_error_code = code;
        Err(error.into())
    }

    /// `fail_protocol` for a code that belongs to an interface OTHER than the
    /// one whose request raised it — every `zxdg_toplevel_decoration_v1` error
    /// is one, since two of the three are raised from the manager's request and
    /// the third from the toplevel's destroy.
    fn fail_protocol_on(&mut self, object: u32, code: u32, error: &str) -> Result<(), String> {
        self.protocol_error_object = Some(object);
        self.fail_protocol(code, error)
    }

    /// td's whole answer on this interface. Sent on creation and again for
    /// every `set_mode`/`unset_mode`, because the protocol makes `configure`
    /// the ONLY way a client learns the mode: one that asked for `client_side`
    /// and heard nothing back would keep drawing its own titlebar.
    fn send_decoration_configure(&mut self, decoration: u32) -> Result<(), String> {
        let mut event = wire::Builder::new();
        event.u32(DECORATION_MODE_SERVER_SIDE);
        self.send(decoration, DECORATION_CONFIGURE, event)
    }

    /// Answer a `set_mode`/`unset_mode` completely.
    ///
    /// The mode event alone is half of what the protocol asks for: a client
    /// applies it on the `xdg_surface.configure` that follows and acknowledges
    /// THAT serial, so a compositor that sent only the first leaves a mapped
    /// window waiting and still drawing its own titlebar. The layout has not
    /// moved, so the configure has to be asked for rather than arising — and it
    /// is asked for through the ordinary path, which is what keeps the serial
    /// the one the tracker is accounting for.
    fn answer_decoration_mode(&mut self, decoration: u32, toplevel: u32) -> Result<(), String> {
        self.send_decoration_configure(decoration)?;
        let Some(Object::XdgToplevel { xdg_surface, .. }) = self.objects.get(&toplevel) else {
            return Ok(());
        };
        let Some(Object::XdgSurface { configure, .. }) = self.objects.get(xdg_surface) else {
            return Ok(());
        };
        configure
            .lock()
            .map_err(|_| "XDG configure tracker lock poisoned".to_string())?
            .reconfigure();
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .wake_layout(self.id);
        Ok(())
    }

    /// Give the toplevel its decoration slot back, so a client may destroy a
    /// decoration and make another. Tolerates a toplevel that is already gone:
    /// the only way to reach that is a client whose toplevel died first, which
    /// is `orphaned` and has already been raised against it.
    fn forget_decoration(&mut self, toplevel: u32) -> Result<(), String> {
        if let Some(Object::XdgToplevel { decoration, .. }) = self.objects.get_mut(&toplevel) {
            *decoration = None;
        }
        Ok(())
    }

    /// `zxdg_decoration_manager_v1.get_toplevel_decoration`.
    ///
    /// The two refusals are both the protocol's, and both are raised before the
    /// object is created so a client that got it wrong is not left holding an
    /// id the compositor never made.
    fn create_toplevel_decoration(&mut self, id: u32, toplevel: u32) -> Result<(), String> {
        let Some(Object::XdgToplevel {
            xdg_surface,
            decoration,
        }) = self.objects.get(&toplevel)
        else {
            return Err(format!(
                "zxdg_decoration_manager_v1 references non-toplevel {toplevel}"
            ));
        };
        // Both are `Copy`, so the toplevel is read without cloning the variant
        // — which for a client naming a `wl_surface` here would have copied its
        // whole pending state to reach a type check.
        let (xdg_surface, decoration) = (*xdg_surface, *decoration);
        if let Some(existing) = decoration {
            return self.fail_protocol_on(
                id,
                DECORATION_ERROR_ALREADY_CONSTRUCTED,
                &format!("xdg_toplevel {toplevel} already has decoration {existing}"),
            );
        }
        if self.toplevel_has_buffer(xdg_surface)? {
            return self.fail_protocol_on(
                id,
                DECORATION_ERROR_UNCONFIGURED_BUFFER,
                &format!(
                    "xdg_toplevel {toplevel} had a buffer before its decoration was configured"
                ),
            );
        }
        self.insert(id, Object::ToplevelDecoration { toplevel })?;
        let Some(Object::XdgToplevel { decoration, .. }) = self.objects.get_mut(&toplevel) else {
            return Err(format!(
                "xdg_toplevel {toplevel} vanished while its decoration was made"
            ));
        };
        *decoration = Some(id);
        // Answered COMPLETELY, the same way a later `set_mode` is. Usually the
        // mapping commit's initial configure would follow this anyway, but a
        // client may commit empty, acknowledge that configure and only then ask
        // — legal, since it has attached no buffer — and its first frame would
        // then be drawn with decorations it never got an ack-able serial for.
        self.answer_decoration_mode(id, toplevel)
    }

    /// Whether the toplevel's `wl_surface` already has pixels, in either of the
    /// two senses the protocol means: a buffer ATTACHED and not yet committed,
    /// which lives in this client's own pending state, and one COMMITTED, which
    /// left here and is the scene's. Asking only the first would miss the
    /// ordinary case — a client that mapped its window and asked for
    /// decorations afterwards.
    fn toplevel_has_buffer(&self, xdg_surface: u32) -> Result<bool, String> {
        let Some(Object::XdgSurface { surface, .. }) = self.objects.get(&xdg_surface) else {
            return Err(format!(
                "xdg_toplevel lost xdg_surface {xdg_surface} before its decoration"
            ));
        };
        let surface = *surface;
        if let Some(Object::Surface(state)) = self.objects.get(&surface) {
            if matches!(state.pending_buffer, Some(PendingBuffer::Buffer { .. })) {
                return Ok(true);
            }
        }
        Ok(self
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .is_mapped(SurfaceKey {
                client: self.id,
                object: surface,
            }))
    }

    fn remove_object(&mut self, id: u32) -> Result<(), String> {
        if id <= 1 {
            return Err(format!("refusing to delete reserved object {id}"));
        }
        if self.objects.remove(&id).is_none() {
            return Err(format!("object {id} does not exist"));
        }
        self.delete_id(id)
    }

    fn insert(&mut self, id: u32, object: Object) -> Result<(), String> {
        if id <= 1 {
            return Err(format!("new object id {id} is reserved"));
        }
        let reservation = self
            .pending_deletes
            .lock()
            .map_err(|_| "pending object deletion lock poisoned".to_string())?
            .get(&id)
            .cloned();
        if let Some(reservation) = reservation {
            let _reservation = reservation
                .lock()
                .map_err(|_| format!("object {id} deletion lock poisoned"))?;
            let pending = self
                .pending_deletes
                .lock()
                .map_err(|_| "pending object deletion lock poisoned".to_string())?
                .contains_key(&id);
            if pending {
                return Err(format!("object id {id} was reused before delete_id"));
            }
        }
        if self.objects.contains_key(&id) {
            return Err(format!("object id {id} was reused before delete_id"));
        }
        if self.objects.len() >= MAX_OBJECTS {
            return Err(format!("client exceeded the {MAX_OBJECTS}-object limit"));
        }
        self.objects.insert(id, object);
        Ok(())
    }

    fn global(
        &mut self,
        registry: u32,
        name: u32,
        interface: &str,
        version: u32,
    ) -> Result<(), String> {
        let mut event = wire::Builder::new();
        event.u32(name);
        event.string(interface)?;
        event.u32(version);
        self.send(registry, 0, event)
    }

    fn advertise_globals(&mut self, registry: u32) -> Result<(), String> {
        self.global(registry, GLOBAL_COMPOSITOR, "wl_compositor", 4)?;
        self.global(registry, GLOBAL_SHM, "wl_shm", 1)?;
        self.global(registry, GLOBAL_OUTPUT, "wl_output", 4)?;
        self.global(registry, GLOBAL_XDG_WM_BASE, "xdg_wm_base", 1)?;
        self.global(registry, GLOBAL_DECORATION, "zxdg_decoration_manager_v1", 1)?;
        self.global(registry, GLOBAL_SEAT, "wl_seat", SEAT_VERSION)
    }

    fn bind_global(
        &mut self,
        name: u32,
        interface: &str,
        version: u32,
        id: u32,
    ) -> Result<(), String> {
        match (name, interface) {
            (GLOBAL_COMPOSITOR, "wl_compositor") if (1..=4).contains(&version) => {
                self.insert(id, Object::Compositor)
            }
            (GLOBAL_SHM, "wl_shm") if version == 1 => {
                self.insert(id, Object::Shm)?;
                for format in [SHM_ARGB8888, SHM_XRGB8888] {
                    let mut event = wire::Builder::new();
                    event.u32(format);
                    self.send(id, 0, event)?;
                }
                Ok(())
            }
            (GLOBAL_OUTPUT, "wl_output") if (1..=4).contains(&version) => {
                self.insert(id, Object::Output { version })?;
                self.send_output(id, version)
            }
            (GLOBAL_XDG_WM_BASE, "xdg_wm_base") if version == 1 => {
                self.insert(id, Object::XdgWmBase)
            }
            (GLOBAL_DECORATION, "zxdg_decoration_manager_v1") if version == 1 => {
                self.insert(id, Object::DecorationManager)
            }
            (GLOBAL_SEAT, "wl_seat") if (1..=SEAT_VERSION).contains(&version) => {
                self.insert(id, Object::Seat { version })?;
                let mut capabilities = wire::Builder::new();
                capabilities.u32(3);
                self.send(id, 0, capabilities)?;
                if version >= 2 {
                    let mut name = wire::Builder::new();
                    name.string("td-seat0")?;
                    self.send(id, 1, name)?;
                }
                Ok(())
            }
            _ => Err(format!(
                "global {name} does not provide {interface} version {version}"
            )),
        }
    }

    fn send_output(&mut self, id: u32, version: u32) -> Result<(), String> {
        let (width, height) = {
            let runtime = self
                .runtime
                .lock()
                .map_err(|_| "runtime lock poisoned".to_string())?;
            (runtime.width(), runtime.height())
        };
        let width = i32::try_from(width).map_err(|_| "output width exceeds i32".to_string())?;
        let height = i32::try_from(height).map_err(|_| "output height exceeds i32".to_string())?;

        let mut geometry = wire::Builder::new();
        geometry.i32(0);
        geometry.i32(0);
        geometry.i32(270);
        geometry.i32(170);
        geometry.i32(0);
        geometry.string("td")?;
        geometry.string("software framebuffer")?;
        geometry.i32(0);
        self.send(id, 0, geometry)?;

        let mut mode = wire::Builder::new();
        mode.u32(3);
        mode.i32(width);
        mode.i32(height);
        mode.i32(60_000);
        self.send(id, 1, mode)?;

        if version >= 2 {
            let mut scale = wire::Builder::new();
            scale.i32(1);
            self.send(id, 3, scale)?;
        }
        if version >= 4 {
            let mut name = wire::Builder::new();
            name.string("TD-1")?;
            self.send(id, 4, name)?;
            let mut description = wire::Builder::new();
            description.string("td software framebuffer")?;
            self.send(id, 5, description)?;
        }
        if version >= 2 {
            self.send(id, 2, wire::Builder::new())?;
        }
        Ok(())
    }

    fn create_pool(
        &mut self,
        id: u32,
        declared_size: i32,
        fds: &mut VecDeque<RawFd>,
    ) -> Result<(), String> {
        if declared_size <= 0 {
            return Err(format!("wl_shm pool size {declared_size} is not positive"));
        }
        let size =
            usize::try_from(declared_size).map_err(|_| "wl_shm pool size overflow".to_string())?;
        if size > MAX_POOL_BYTES {
            return Err(format!("wl_shm pool size {size} exceeds {MAX_POOL_BYTES}"));
        }
        let fd = fds
            .pop_front()
            .ok_or_else(|| "wl_shm.create_pool arrived without a descriptor".to_string())?;
        let file = sys::duplicate_received(fd)?;
        let actual = usize::try_from(
            file.metadata()
                .map_err(|e| format!("stat wl_shm pool: {e}"))?
                .len(),
        )
        .map_err(|_| "wl_shm backing file is too large".to_string())?;
        if size > actual {
            return Err(format!(
                "wl_shm declared {size} bytes but backing file has {actual}"
            ));
        }
        self.insert(
            id,
            Object::Pool(Pool {
                file: Arc::new(file),
                size,
            }),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_buffer(
        &mut self,
        pool: Pool,
        id: u32,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: u32,
    ) -> Result<(), String> {
        if offset < 0 || width <= 0 || height <= 0 || stride <= 0 {
            return Err(format!(
                "invalid wl_shm buffer offset={offset} width={width} height={height} stride={stride}"
            ));
        }
        let offset = usize::try_from(offset).map_err(|_| "buffer offset overflow".to_string())?;
        let width = usize::try_from(width).map_err(|_| "buffer width overflow".to_string())?;
        let height = usize::try_from(height).map_err(|_| "buffer height overflow".to_string())?;
        let stride = usize::try_from(stride).map_err(|_| "buffer stride overflow".to_string())?;
        if width > MAX_UI_DIMENSION || height > MAX_UI_DIMENSION {
            return Err(format!(
                "wl_shm buffer {width}x{height} exceeds the dimension limit"
            ));
        }
        if !matches!(format, SHM_ARGB8888 | SHM_XRGB8888) {
            return Err(format!("unsupported wl_shm format {format}"));
        }
        let row = width
            .checked_mul(4)
            .ok_or_else(|| "wl_shm row size overflow".to_string())?;
        if stride < row {
            return Err(format!(
                "wl_shm stride {stride} is smaller than the {row}-byte row"
            ));
        }
        let final_row = height
            .saturating_sub(1)
            .checked_mul(stride)
            .and_then(|value| value.checked_add(row))
            .and_then(|value| value.checked_add(offset))
            .ok_or_else(|| "wl_shm buffer extent overflow".to_string())?;
        if final_row > pool.size {
            return Err(format!(
                "wl_shm buffer ends at {final_row}, beyond pool size {}",
                pool.size
            ));
        }
        let tight = row
            .checked_mul(height)
            .ok_or_else(|| "wl_shm copied surface size overflow".to_string())?;
        if tight > MAX_POOL_BYTES {
            return Err(format!(
                "wl_shm copied surface exceeds {MAX_POOL_BYTES} bytes"
            ));
        }
        self.insert(
            id,
            Object::Buffer(Buffer {
                serial: NEXT_BUFFER_SERIAL.fetch_add(1, Ordering::Relaxed),
                file: pool.file,
                offset,
                width,
                height,
                stride,
                format,
            }),
        )
    }

    fn copy_buffer(buffer: &Buffer) -> Result<Surface, String> {
        let row = buffer
            .width
            .checked_mul(4)
            .ok_or_else(|| "surface row overflow".to_string())?;
        let total = row
            .checked_mul(buffer.height)
            .ok_or_else(|| "surface size overflow".to_string())?;
        let mut pixels = vec![0; total];
        for source_y in 0..buffer.height {
            let source = buffer
                .offset
                .checked_add(
                    source_y
                        .checked_mul(buffer.stride)
                        .ok_or_else(|| "source row overflow".to_string())?,
                )
                .ok_or_else(|| "source offset overflow".to_string())?;
            let target = source_y
                .checked_mul(row)
                .ok_or_else(|| "target row overflow".to_string())?;
            let target_end = target
                .checked_add(row)
                .ok_or_else(|| "target row end overflow".to_string())?;
            let destination = pixels
                .get_mut(target..target_end)
                .ok_or_else(|| "target row escaped surface".to_string())?;
            buffer
                .file
                .read_exact_at(
                    destination,
                    u64::try_from(source).map_err(|_| "source offset exceeds u64".to_string())?,
                )
                .map_err(|e| format!("read wl_shm row {source_y}: {e}"))?;
        }
        Ok(Surface {
            width: buffer.width,
            height: buffer.height,
            pixels,
            format: buffer.format,
        })
    }

    fn commit_surface(&mut self, id: u32, mut state: SurfaceState) -> Result<(), String> {
        let input_region_changed = if let Some(region) = state.pending_input_region.take() {
            state.input_region = region;
            true
        } else {
            false
        };
        let input_region = state.input_region.clone();
        let attaching_buffer = matches!(state.pending_buffer, Some(PendingBuffer::Buffer { .. }));
        let was_mapped = self.mapped_bytes.contains_key(&id);
        let cursor = state.role == Some(SurfaceRole::Cursor);
        let mut xdg_configure = None;
        if let Some(SurfaceRole::Xdg(role)) = state.role {
            let xdg = self
                .objects
                .get(&role)
                .cloned()
                .ok_or_else(|| format!("wl_surface {id} has a destroyed xdg_surface role"))?;
            let Object::XdgSurface {
                toplevel,
                configure,
                ..
            } = xdg
            else {
                return Err(format!("wl_surface {id} has a non-XDG role object"));
            };
            let toplevel =
                toplevel.ok_or_else(|| format!("xdg_surface {role} has no role object"))?;
            xdg_configure = Some(Arc::clone(&configure));
            let initial_sent = configure
                .lock()
                .map_err(|_| "XDG configure tracker lock poisoned".to_string())?
                .initial_sent();
            if !initial_sent {
                if attaching_buffer {
                    return Err(format!(
                        "xdg_surface {role} attached a buffer before its initial configure"
                    ));
                }
                send_initial_configure(
                    &self.outbound,
                    &ConfigureRegistration {
                        xdg_surface: role,
                        toplevel,
                        tracker: Arc::clone(&configure),
                    },
                )?;
            } else if attaching_buffer
                && !configure
                    .lock()
                    .map_err(|_| "XDG configure tracker lock poisoned".to_string())?
                    .can_attach()
            {
                return Err(format!(
                    "xdg_surface {role} attached a buffer before acknowledging configure"
                ));
            }
        } else if state.role.is_none() && attaching_buffer {
            return Err(format!("wl_surface {id} attached a buffer without a role"));
        }
        if let Some(pending) = state.pending_buffer {
            let key = SurfaceKey {
                client: self.id,
                object: id,
            };
            if cursor {
                match pending {
                    // A cursor surface with its buffer taken away has no
                    // image to be, so td's own cross stands. Deliberately NOT
                    // read as "hide": a client asking for no cursor says so
                    // with a null SURFACE, which cannot be confused with a
                    // client between two frames of an animated one.
                    PendingBuffer::Detach => self
                        .runtime
                        .lock()
                        .map_err(|_| "runtime lock poisoned".to_string())?
                        .detach_cursor(key)?,
                    PendingBuffer::Buffer { object, buffer } => {
                        // The dimension bound is applied HERE rather than at
                        // the scene, because `copy_buffer` allocates and
                        // reads the whole image: refusing it afterwards
                        // bounds what is RETAINED and not what is spent, so
                        // every connected client could make td materialise a
                        // buffer of full output size at once. The scene
                        // checks again on what it keeps; this is what stops
                        // the copy from happening at all.
                        if buffer.width <= MAX_CURSOR_DIMENSION
                            && buffer.height <= MAX_CURSOR_DIMENSION
                        {
                            let image = Self::copy_buffer(&buffer)?;
                            self.runtime
                                .lock()
                                .map_err(|_| "runtime lock poisoned".to_string())?
                                .commit_cursor(key, image)?;
                        } else {
                            // Still a REPLACEMENT: the surface's contents are
                            // now something td will not draw, so the frame it
                            // held is one the client has superseded.
                            self.runtime
                                .lock()
                                .map_err(|_| "runtime lock poisoned".to_string())?
                                .detach_cursor(key)?;
                        }
                        // Released whether or not the image was KEPT. A
                        // cursor over the scene's bound is refused, and a
                        // client left waiting on the buffer it would have
                        // reused is a client that stops drawing — a worse
                        // failure than the cursor it asked for not appearing.
                        if matches!(
                            self.objects.get(&object),
                            Some(Object::Buffer(current)) if current.serial == buffer.serial
                        ) {
                            self.send(object, 0, wire::Builder::new())?;
                        }
                    }
                }
            } else {
                match pending {
                    PendingBuffer::Detach => {
                        if was_mapped {
                            if let Some(configure) = xdg_configure {
                                configure
                                    .lock()
                                    .map_err(|_| "XDG configure tracker lock poisoned".to_string())?
                                    .unmap()?;
                            }
                        }
                        self.unmap_surface(id)?;
                    }
                    PendingBuffer::Buffer { object, buffer } => {
                        let surface_bytes = buffer
                            .width
                            .checked_mul(buffer.height)
                            .and_then(|pixels| pixels.checked_mul(4))
                            .ok_or_else(|| "client surface byte count overflow".to_string())?;
                        let prior = self.mapped_bytes.get(&id).copied().unwrap_or(0);
                        let next = client_surface_total(self.mapped_total, prior, surface_bytes)?;
                        let surface = Self::copy_buffer(&buffer)?;
                        self.runtime
                            .lock()
                            .map_err(|_| "runtime lock poisoned".to_string())?
                            .commit_with_input_region(key, surface, input_region.clone())?;
                        self.mapped_bytes.insert(id, surface_bytes);
                        self.mapped_total = next;
                        if matches!(
                            self.objects.get(&object),
                            Some(Object::Buffer(current)) if current.serial == buffer.serial
                        ) {
                            self.send(object, 0, wire::Builder::new())?;
                        }
                    }
                }
            }
        } else if input_region_changed && !cursor {
            self.runtime
                .lock()
                .map_err(|_| "runtime lock poisoned".to_string())?
                .set_input_region(
                    SurfaceKey {
                        client: self.id,
                        object: id,
                    },
                    input_region.clone(),
                )?;
        }
        for callback in state.frame_callbacks {
            let mut done = wire::Builder::new();
            done.u32(next_serial());
            self.send(callback, 0, done)?;
            self.objects.remove(&callback);
            self.delete_id(callback)?;
        }
        if let Some(Object::Surface(current)) = self.objects.get_mut(&id) {
            current.pending_buffer = None;
            current.pending_input_region = None;
            current.input_region = input_region;
            current.frame_callbacks.clear();
        }
        Ok(())
    }

    fn retained_input_region_operations(&self) -> usize {
        let mut seen = BTreeSet::new();
        let mut retained = 0usize;
        let mut count = |region: &SharedInputRegion| {
            let identity = Arc::as_ptr(region) as usize;
            if seen.insert(identity) {
                retained = retained.saturating_add(region.len());
            }
        };
        for object in self.objects.values() {
            match object {
                Object::Region(region) => count(region),
                Object::Surface(surface) => {
                    if let Some(region) = &surface.input_region {
                        count(region);
                    }
                    if let Some(Some(region)) = &surface.pending_input_region {
                        count(region);
                    }
                }
                _ => {}
            }
        }
        retained
    }

    fn dispatch_region(&mut self, message: &wire::Message) -> Result<(), String> {
        let mut args = wire::Cursor::new(&message.payload);
        match message.opcode {
            0 => {
                args.finish()?;
                self.remove_object(message.object)
            }
            1 | 2 => {
                let x = args.i32()?;
                let y = args.i32()?;
                let width = args.i32()?;
                let height = args.i32()?;
                args.finish()?;
                let Some(Object::Region(region)) = self.objects.get(&message.object) else {
                    return Err(format!("request for non-region object {}", message.object));
                };
                if width <= 0 || height <= 0 || region.len() >= MAX_INPUT_REGION_OPERATIONS {
                    return Ok(());
                }
                let additional = if Arc::strong_count(region) > 1 {
                    region.len().saturating_add(1)
                } else {
                    1
                };
                if self
                    .retained_input_region_operations()
                    .saturating_add(additional)
                    > MAX_CLIENT_INPUT_REGION_OPERATIONS
                {
                    return Ok(());
                }
                let Some(Object::Region(region)) = self.objects.get_mut(&message.object) else {
                    return Err(format!("request for non-region object {}", message.object));
                };
                let region = Arc::make_mut(region);
                if message.opcode == 1 {
                    region.add(x, y, width, height);
                } else {
                    region.subtract(x, y, width, height);
                }
                Ok(())
            }
            _ => Err(format!("unsupported wl_region request {}", message.opcode)),
        }
    }

    fn dispatch(
        &mut self,
        message: wire::Message,
        fds: &mut VecDeque<RawFd>,
    ) -> Result<(), String> {
        self.protocol_error_code = WL_DISPLAY_ERROR_IMPLEMENTATION;
        self.protocol_error_object = None;
        if matches!(self.objects.get(&message.object), Some(Object::Region(_))) {
            return self.dispatch_region(&message);
        }
        let object = self
            .objects
            .get(&message.object)
            .cloned()
            .ok_or_else(|| format!("request for unknown object {}", message.object))?;
        let mut args = wire::Cursor::new(&message.payload);
        match object {
            Object::Display => match message.opcode {
                0 => {
                    let callback = args.u32()?;
                    args.finish()?;
                    self.insert(callback, Object::Callback)?;
                    let mut done = wire::Builder::new();
                    done.u32(next_serial());
                    self.send(callback, 0, done)?;
                    self.objects.remove(&callback);
                    self.delete_id(callback)
                }
                1 => {
                    let registry = args.u32()?;
                    args.finish()?;
                    self.insert(registry, Object::Registry)?;
                    self.advertise_globals(registry)
                }
                _ => Err(format!("unsupported wl_display request {}", message.opcode)),
            },
            Object::Registry => match message.opcode {
                0 => {
                    let name = args.u32()?;
                    let interface = args.string()?;
                    let version = args.u32()?;
                    let id = args.u32()?;
                    args.finish()?;
                    self.bind_global(name, &interface, version, id)
                }
                _ => Err(format!(
                    "unsupported wl_registry request {}",
                    message.opcode
                )),
            },
            Object::Compositor => match message.opcode {
                0 => {
                    let surface = args.u32()?;
                    args.finish()?;
                    self.insert(surface, Object::Surface(SurfaceState::default()))
                }
                1 => {
                    let region = args.u32()?;
                    args.finish()?;
                    self.insert(region, Object::Region(Arc::new(InputRegion::new())))
                }
                _ => Err(format!(
                    "unsupported wl_compositor request {}",
                    message.opcode
                )),
            },
            Object::Region(_) => Err("wl_region dispatch bypassed its bounded path".into()),
            Object::Shm => match message.opcode {
                0 => {
                    let id = args.u32()?;
                    let size = args.i32()?;
                    args.finish()?;
                    self.create_pool(id, size, fds)
                }
                _ => Err(format!("unsupported wl_shm request {}", message.opcode)),
            },
            Object::Pool(pool) => match message.opcode {
                0 => {
                    let id = args.u32()?;
                    let offset = args.i32()?;
                    let width = args.i32()?;
                    let height = args.i32()?;
                    let stride = args.i32()?;
                    let format = args.u32()?;
                    args.finish()?;
                    self.create_buffer(pool, id, offset, width, height, stride, format)
                }
                1 => {
                    args.finish()?;
                    self.remove_object(message.object)
                }
                2 => {
                    let size = args.i32()?;
                    args.finish()?;
                    if size <= 0 {
                        return Err(format!("invalid wl_shm pool resize {size}"));
                    }
                    let size = usize::try_from(size)
                        .map_err(|_| "wl_shm pool resize overflow".to_string())?;
                    let actual = usize::try_from(
                        pool.file
                            .metadata()
                            .map_err(|e| format!("stat resized wl_shm pool: {e}"))?
                            .len(),
                    )
                    .map_err(|_| "resized wl_shm file is too large".to_string())?;
                    if size < pool.size || size > actual || size > MAX_POOL_BYTES {
                        return Err(format!(
                            "wl_shm pool resize {size} is outside {}..={}",
                            pool.size,
                            actual.min(MAX_POOL_BYTES)
                        ));
                    }
                    self.objects.insert(
                        message.object,
                        Object::Pool(Pool {
                            file: pool.file,
                            size,
                        }),
                    );
                    Ok(())
                }
                _ => Err(format!(
                    "unsupported wl_shm_pool request {}",
                    message.opcode
                )),
            },
            Object::Buffer(_) => match message.opcode {
                0 => {
                    args.finish()?;
                    self.remove_object(message.object)
                }
                _ => Err(format!("unsupported wl_buffer request {}", message.opcode)),
            },
            Object::Surface(mut state) => match message.opcode {
                0 => {
                    args.finish()?;
                    if state.role.is_some_and(|role| {
                        matches!(role, SurfaceRole::Xdg(object) if self.objects.contains_key(&object))
                    })
                    {
                        return Err(format!(
                            "wl_surface {} was destroyed before its role object",
                            message.object
                        ));
                    }
                    self.remove_surface(message.object)?;
                    self.remove_surface_object(message.object)
                }
                1 => {
                    let buffer = args.u32()?;
                    args.i32()?;
                    args.i32()?;
                    args.finish()?;
                    state.pending_buffer = if buffer == 0 {
                        Some(PendingBuffer::Detach)
                    } else {
                        let buffer_state = match self.objects.get(&buffer).cloned() {
                            Some(Object::Buffer(buffer_state)) => buffer_state,
                            _ => {
                                return Err(format!(
                                    "surface attach references non-buffer {buffer}"
                                ))
                            }
                        };
                        Some(PendingBuffer::Buffer {
                            object: buffer,
                            buffer: buffer_state,
                        })
                    };
                    self.objects.insert(message.object, Object::Surface(state));
                    Ok(())
                }
                2 | 9 => {
                    for _ in 0..4 {
                        args.i32()?;
                    }
                    args.finish()
                }
                3 => {
                    let callback = args.u32()?;
                    args.finish()?;
                    self.insert(callback, Object::Callback)?;
                    state.frame_callbacks.push(callback);
                    self.objects.insert(message.object, Object::Surface(state));
                    Ok(())
                }
                4 => {
                    let region = args.u32()?;
                    args.finish()?;
                    if region != 0 && !matches!(self.objects.get(&region), Some(Object::Region(_)))
                    {
                        return Err(format!("surface references non-region {region}"));
                    }
                    Ok(())
                }
                5 => {
                    let region = args.u32()?;
                    args.finish()?;
                    state.pending_input_region = if region == 0 {
                        Some(None)
                    } else {
                        match self.objects.get(&region) {
                            Some(Object::Region(region)) => Some(Some(region.clone())),
                            _ => return Err(format!("surface references non-region {region}")),
                        }
                    };
                    self.objects.insert(message.object, Object::Surface(state));
                    Ok(())
                }
                6 => {
                    args.finish()?;
                    self.commit_surface(message.object, state)
                }
                7 => {
                    let transform = args.i32()?;
                    args.finish()?;
                    if transform != 0 {
                        return Err(format!("unsupported buffer transform {transform}"));
                    }
                    Ok(())
                }
                8 => {
                    let scale = args.i32()?;
                    args.finish()?;
                    if scale != 1 {
                        return Err(format!("unsupported buffer scale {scale}"));
                    }
                    Ok(())
                }
                _ => Err(format!("unsupported wl_surface request {}", message.opcode)),
            },
            Object::Callback => Err(format!(
                "wl_callback object {} accepts no requests",
                message.object
            )),
            Object::Output { version } => match message.opcode {
                0 if version >= 3 => {
                    args.finish()?;
                    self.remove_object(message.object)
                }
                _ => Err(format!("unsupported wl_output request {}", message.opcode)),
            },
            Object::Seat { version } => match message.opcode {
                0 => {
                    let pointer = args.u32()?;
                    args.finish()?;
                    self.create_pointer(pointer, version)
                }
                1 => {
                    let keyboard = args.u32()?;
                    args.finish()?;
                    self.create_keyboard(keyboard, version)
                }
                3 if version >= 5 => {
                    args.finish()?;
                    self.remove_object(message.object)
                }
                2 => {
                    args.u32()?;
                    args.finish()?;
                    self.fail_protocol(
                        WL_SEAT_ERROR_MISSING_CAPABILITY,
                        "wl_touch is not supported",
                    )
                }
                _ => Err(format!("unsupported wl_seat request {}", message.opcode)),
            },
            Object::Keyboard { version } => match message.opcode {
                0 if version >= 3 => {
                    args.finish()?;
                    self.remove_keyboard(message.object)
                }
                _ => Err(format!(
                    "unsupported wl_keyboard request {}",
                    message.opcode
                )),
            },
            Object::Pointer { version } => match message.opcode {
                0 => {
                    let serial = args.u32()?;
                    let surface = args.u32()?;
                    let hotspot_x = args.i32()?;
                    let hotspot_y = args.i32()?;
                    args.finish()?;
                    let authority = *self
                        .pointer_authority
                        .lock()
                        .map_err(|_| "pointer authority lock poisoned".to_string())?;
                    // The focus check and the install that follows it happen
                    // under ONE hold of the runtime lock. Released between
                    // them, a pointer report could move focus in the gap, and
                    // the cursor would then be installed by a client that no
                    // longer has one — where it would stay until some later
                    // report happened to move focus again. Held through a
                    // cloned handle rather than `self.runtime` so the guard
                    // borrows nothing of `self`, leaving the role assignment
                    // and `fail_protocol` reachable inside it.
                    let runtime = Arc::clone(&self.runtime);
                    let mut runtime = runtime
                        .lock()
                        .map_err(|_| "runtime lock poisoned".to_string())?;
                    let focus = runtime.pointer_snapshot().focus;
                    let authorized = authority.is_some_and(|candidate| {
                        candidate.serial == serial
                            && focus.is_some_and(|target| {
                                target.surface.client == self.id
                                    && target.surface.object == candidate.surface
                            })
                    });
                    if !authorized {
                        return Ok(());
                    }
                    if surface == 0 {
                        // A null surface is the request for NO cursor, and it
                        // is the whole of what this call means: there is no
                        // surface to give a role to and no commit to wait for.
                        return runtime.set_cursor(self.id, None);
                    }
                    let mut state = match self.objects.get(&surface).cloned() {
                        Some(Object::Surface(state)) => state,
                        _ => {
                            return Err(format!(
                                "wl_pointer cursor references non-surface {surface}"
                            ))
                        }
                    };
                    match state.role {
                        None => state.role = Some(SurfaceRole::Cursor),
                        Some(SurfaceRole::Cursor) => {}
                        Some(SurfaceRole::Xdg(_)) => {
                            drop(runtime);
                            return self.fail_protocol(
                                WL_POINTER_ERROR_ROLE,
                                &format!("wl_surface {surface} already has an incompatible role"),
                            );
                        }
                    }
                    self.objects.insert(surface, Object::Surface(state));
                    runtime.set_cursor(
                        self.id,
                        Some(CursorRequest {
                            surface,
                            hotspot_x,
                            hotspot_y,
                        }),
                    )
                }
                1 if version >= 3 => {
                    args.finish()?;
                    self.remove_pointer(message.object)
                }
                _ => Err(format!("unsupported wl_pointer request {}", message.opcode)),
            },
            Object::XdgWmBase => match message.opcode {
                0 => {
                    args.finish()?;
                    self.remove_object(message.object)
                }
                2 => {
                    let id = args.u32()?;
                    let surface = args.u32()?;
                    args.finish()?;
                    let mut state = match self.objects.get(&surface).cloned() {
                        Some(Object::Surface(state)) => state,
                        _ => {
                            return Err(format!(
                                "xdg_surface role references non-surface {surface}"
                            ))
                        }
                    };
                    if state.role.is_some() {
                        return Err(format!("wl_surface {surface} already has a role"));
                    }
                    self.insert(
                        id,
                        Object::XdgSurface {
                            surface,
                            toplevel: None,
                            configure: Arc::new(Mutex::new(ConfigureTracker::new())),
                        },
                    )?;
                    state.role = Some(SurfaceRole::Xdg(id));
                    self.objects.insert(surface, Object::Surface(state));
                    Ok(())
                }
                3 => {
                    args.u32()?;
                    args.finish()
                }
                1 => Err("xdg_positioner is not supported".into()),
                _ => Err(format!(
                    "unsupported xdg_wm_base request {}",
                    message.opcode
                )),
            },
            Object::XdgSurface {
                surface,
                toplevel,
                configure,
            } => match message.opcode {
                0 => {
                    args.finish()?;
                    if toplevel.is_some() {
                        return Err(format!(
                            "xdg_surface {} was destroyed before its xdg_toplevel",
                            message.object
                        ));
                    }
                    self.remove_object(message.object)
                }
                1 => {
                    let new_toplevel = args.u32()?;
                    args.finish()?;
                    if toplevel.is_some() {
                        return Err(format!(
                            "xdg_surface {} already has a role object",
                            message.object
                        ));
                    }
                    if !matches!(self.objects.get(&surface), Some(Object::Surface(_))) {
                        return Err(format!(
                            "xdg_surface refers to missing wl_surface {surface}"
                        ));
                    }
                    self.insert(
                        new_toplevel,
                        Object::XdgToplevel {
                            xdg_surface: message.object,
                            decoration: None,
                        },
                    )?;
                    let key = SurfaceKey {
                        client: self.id,
                        object: surface,
                    };
                    let prior = self
                        .configurations
                        .lock()
                        .map_err(|_| "configure registration lock poisoned".to_string())?
                        .insert(
                            key,
                            ConfigureRegistration {
                                xdg_surface: message.object,
                                toplevel: new_toplevel,
                                tracker: Arc::clone(&configure),
                            },
                        );
                    if prior.is_some() {
                        return Err(format!(
                            "wl_surface {surface} already has a configure registration"
                        ));
                    }
                    self.objects.insert(
                        message.object,
                        Object::XdgSurface {
                            surface,
                            toplevel: Some(new_toplevel),
                            configure,
                        },
                    );
                    Ok(())
                }
                3 => {
                    for _ in 0..4 {
                        args.i32()?;
                    }
                    args.finish()
                }
                4 => {
                    let serial = args.u32()?;
                    args.finish()?;
                    configure
                        .lock()
                        .map_err(|_| "XDG configure tracker lock poisoned".to_string())?
                        .acknowledge(serial)
                        .map_err(|error| format!("xdg_surface {} {error}", message.object))?;
                    self.runtime
                        .lock()
                        .map_err(|_| "runtime lock poisoned".to_string())?
                        .wake_layout(self.id);
                    Ok(())
                }
                2 => Err("xdg_popup is not supported".into()),
                _ => Err(format!(
                    "unsupported xdg_surface request {}",
                    message.opcode
                )),
            },
            Object::XdgToplevel {
                xdg_surface,
                decoration,
            } => match message.opcode {
                0 => {
                    args.finish()?;
                    // The decoration object outliving the thing it decorates is
                    // the protocol's own `orphaned`, and it is checked BEFORE
                    // anything is torn down: the client is being told its
                    // destroy order was wrong, so the toplevel it named must
                    // still be there to be named in the diagnostic.
                    if let Some(decoration) = decoration {
                        return self.fail_protocol_on(
                            decoration,
                            DECORATION_ERROR_ORPHANED,
                            &format!(
                                "xdg_toplevel {} destroyed before its decoration {decoration}",
                                message.object
                            ),
                        );
                    }
                    let Some(Object::XdgSurface { surface, .. }) =
                        self.objects.get(&xdg_surface).cloned()
                    else {
                        return Err(format!(
                            "xdg_toplevel {} lost xdg_surface {xdg_surface}",
                            message.object
                        ));
                    };
                    self.unregister_surface(surface)?;
                    self.remove_object(message.object)?;
                    self.unmap_surface(surface)?;
                    // The title is this TOPLEVEL's, and unmapping the surface
                    // no longer drops one — a toplevel created on the same
                    // wl_surface next would inherit the name of the dead one.
                    self.runtime
                        .lock()
                        .map_err(|_| "runtime lock poisoned".to_string())?
                        .forget_title(SurfaceKey {
                            client: self.id,
                            object: surface,
                        })?;
                    self.objects.insert(
                        xdg_surface,
                        Object::XdgSurface {
                            surface,
                            toplevel: None,
                            configure: Arc::new(Mutex::new(ConfigureTracker::new())),
                        },
                    );
                    Ok(())
                }
                1 => {
                    args.u32()?;
                    args.finish()
                }
                // set_title. Kept, where set_app_id below is still read for
                // wire validity and dropped: the title is what a title bar
                // shows, and an app id is not.
                2 => {
                    let title = args.string()?;
                    args.finish()?;
                    let Some(Object::XdgSurface { surface, .. }) =
                        self.objects.get(&xdg_surface).cloned()
                    else {
                        return Err(format!(
                            "xdg_toplevel {} lost xdg_surface {xdg_surface}",
                            message.object
                        ));
                    };
                    self.runtime
                        .lock()
                        .map_err(|_| "runtime lock poisoned".to_string())?
                        .set_title(
                            SurfaceKey {
                                client: self.id,
                                object: surface,
                            },
                            title,
                        )
                }
                3 => {
                    args.string()?;
                    args.finish()
                }
                7 | 8 => {
                    args.i32()?;
                    args.i32()?;
                    args.finish()
                }
                9 | 10 | 12 | 13 => args.finish(),
                4 | 5 | 6 | 11 => Err(format!(
                    "interactive xdg_toplevel request {} is not supported",
                    message.opcode
                )),
                _ => Err(format!(
                    "unsupported xdg_toplevel request {}",
                    message.opcode
                )),
            },
            Object::DecorationManager => match message.opcode {
                0 => {
                    args.finish()?;
                    self.remove_object(message.object)
                }
                1 => {
                    let id = args.u32()?;
                    let toplevel = args.u32()?;
                    args.finish()?;
                    self.create_toplevel_decoration(id, toplevel)
                }
                _ => Err(format!(
                    "unsupported zxdg_decoration_manager_v1 request {}",
                    message.opcode
                )),
            },
            Object::ToplevelDecoration { toplevel } => match message.opcode {
                0 => {
                    args.finish()?;
                    self.forget_decoration(toplevel)?;
                    self.remove_object(message.object)
                }
                // set_mode. The client states a PREFERENCE and the compositor
                // answers with what it will actually do, which the protocol
                // says need not agree. td's answer never does depend on it, so
                // the mode is read for wire validity and to refuse a value the
                // enum does not define — a client that meant `server_side` and
                // sent 0 should hear about it rather than be told yes.
                1 => {
                    let mode = args.u32()?;
                    args.finish()?;
                    if mode != DECORATION_MODE_SERVER_SIDE && mode != DECORATION_MODE_CLIENT_SIDE {
                        return Err(format!(
                            "zxdg_toplevel_decoration_v1 {} asked for undefined mode {mode}",
                            message.object
                        ));
                    }
                    self.answer_decoration_mode(message.object, toplevel)
                }
                2 => {
                    args.finish()?;
                    self.answer_decoration_mode(message.object, toplevel)
                }
                _ => Err(format!(
                    "unsupported zxdg_toplevel_decoration_v1 request {}",
                    message.opcode
                )),
            },
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum DispatchOutcome {
    NeedInput,
    Disconnected,
}

fn dispatch_buffered(
    client: &mut Client,
    bytes: &mut Vec<u8>,
    fds: &mut VecDeque<RawFd>,
) -> Result<DispatchOutcome, String> {
    loop {
        let message = match wire::take(bytes) {
            Ok(Some(message)) => message,
            Ok(None) => return Ok(DispatchOutcome::NeedInput),
            Err(error) => {
                client.protocol_error(1, WL_DISPLAY_ERROR_IMPLEMENTATION, &error);
                return Err(error);
            }
        };
        let object = message.object;
        if let Err(error) = client.dispatch(message, fds) {
            let object = client.protocol_error_object.unwrap_or(object);
            client.protocol_error(object, client.protocol_error_code, &error);
            return Err(error);
        }
        if client.disconnected()? {
            return Ok(DispatchOutcome::Disconnected);
        }
    }
}

fn serve_client(
    stream: UnixStream,
    id: u64,
    runtime: Arc<Mutex<Runtime>>,
    keymap: KeymapFile,
) -> Result<(), String> {
    let mut client = Client::new(id, stream, Arc::clone(&runtime), keymap)?;
    let subscription = runtime
        .lock()
        .map_err(|_| "runtime lock poisoned".to_string())?
        .subscribe(id)?;
    let (receiver, stop) = subscription.split();
    let keyboard_subscription = match runtime.lock() {
        Ok(mut runtime) => runtime.subscribe_input_with_activity(
            id,
            Arc::clone(&client.keyboard_active),
            Arc::clone(&client.pointer_active),
        ),
        Err(poisoned) => {
            stop.stop();
            poisoned.into_inner().unsubscribe(id);
            return Err("runtime lock poisoned".into());
        }
    };
    let keyboard_subscription = match keyboard_subscription {
        Ok(subscription) => subscription,
        Err(error) => {
            stop.stop();
            if let Ok(mut runtime) = runtime.lock() {
                runtime.unsubscribe(id);
            }
            return Err(error);
        }
    };
    let (keyboard_receiver, keyboard_stop) = keyboard_subscription.split();
    let worker_stop = stop.clone();
    let worker_runtime = Arc::clone(&runtime);
    let configurations = Arc::clone(&client.configurations);
    let outbound = Arc::clone(&client.outbound);
    let worker_outbound = Arc::clone(&outbound);
    let configure_thread = match thread::Builder::new()
        .name(format!("wayland-configure-{id}"))
        .spawn(move || {
            let result = configure_worker(
                receiver,
                worker_stop,
                worker_runtime,
                configurations,
                worker_outbound,
            );
            if let Err(error) = &result {
                eprintln!("td-compositor: client {id} configure: {error}");
                if let Ok(mut outbound) = outbound.lock() {
                    outbound.disconnect();
                }
            }
            result
        }) {
        Ok(worker) => worker,
        Err(error) => {
            stop.stop();
            keyboard_stop.stop();
            if let Ok(mut runtime) = runtime.lock() {
                runtime.unsubscribe(id);
                runtime.unsubscribe_keyboard(id);
            }
            return Err(format!("spawn Wayland configure worker {id}: {error}"));
        }
    };
    let worker_seat_stop = keyboard_stop.clone();
    let keyboards = Arc::clone(&client.keyboards);
    let pointers = Arc::clone(&client.pointers);
    let pointer_authority = Arc::clone(&client.pointer_authority);
    let pending_deletes = Arc::clone(&client.pending_deletes);
    let outbound = Arc::clone(&client.outbound);
    let worker_outbound = Arc::clone(&outbound);
    let seat_thread = match thread::Builder::new()
        .name(format!("wayland-seat-{id}"))
        .spawn(move || {
            supervise_seat_worker(
                id,
                keyboard_receiver,
                worker_seat_stop,
                keyboards,
                pointers,
                pointer_authority,
                pending_deletes,
                worker_outbound,
            )
        }) {
        Ok(worker) => worker,
        Err(error) => {
            if let Ok(mut outbound) = client.outbound.lock() {
                outbound.disconnect();
            }
            stop.stop();
            keyboard_stop.stop();
            if let Ok(mut runtime) = runtime.lock() {
                runtime.unsubscribe(id);
                runtime.unsubscribe_keyboard(id);
            }
            let _ = configure_thread.join();
            return Err(format!("spawn Wayland seat worker {id}: {error}"));
        }
    };
    let mut bytes = Vec::with_capacity(64 * 1024);
    let mut incoming = [0u8; 64 * 1024];
    let mut fds: VecDeque<RawFd> = VecDeque::new();
    let outcome = loop {
        match dispatch_buffered(&mut client, &mut bytes, &mut fds) {
            Ok(DispatchOutcome::NeedInput) => {}
            Ok(DispatchOutcome::Disconnected) => break Ok(()),
            Err(error) => break Err(error),
        }
        if bytes.len() > MAX_CLIENT_BUFFER {
            break Err(format!(
                "client receive buffer exceeded {MAX_CLIENT_BUFFER} bytes"
            ));
        }
        let received = match sys::recv_with_fds(&client.stream, &mut incoming) {
            Ok(value) => value,
            Err(sys::ReceiveError::Disconnected) => break Ok(()),
            Err(sys::ReceiveError::TimedOut) => {
                break Err("recvmsg: unexpected Wayland receive timeout".into())
            }
            Err(sys::ReceiveError::Failure(error)) => break Err(error),
        };
        if received.count == 0 {
            break Ok(());
        }
        let Some(received_bytes) = incoming.get(..received.count) else {
            break Err("recvmsg byte count escaped input buffer".to_string());
        };
        bytes.extend_from_slice(received_bytes);
        fds.extend(received.fds);
        if fds.len() > MAX_PENDING_FDS {
            break Err(format!(
                "client queued more than {MAX_PENDING_FDS} descriptors"
            ));
        }
    };
    let raw: Vec<RawFd> = fds.into_iter().collect();
    sys::discard_received(&raw);
    let _ = client.stream.shutdown(Shutdown::Both);
    stop.stop();
    keyboard_stop.stop();
    let cleanup = match runtime.lock() {
        Ok(mut runtime) => {
            let cleanup = runtime.remove_client(id);
            runtime.unsubscribe(id);
            runtime.unsubscribe_keyboard(id);
            cleanup
        }
        Err(_) => Err("runtime lock poisoned".to_string()),
    };
    let joined = configure_thread
        .join()
        .map_err(|_| format!("Wayland configure worker {id} panicked"))
        .and_then(|result| result);
    let seat_joined = seat_thread
        .join()
        .map_err(|_| format!("Wayland seat worker {id} panicked"))
        .and_then(|result| result);
    let mut errors = Vec::new();
    for result in [outcome, cleanup, joined, seat_joined] {
        if let Err(error) = result {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn keymap_directory(path: &Path) -> Result<&Path, String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| format!("Wayland socket {} has no parent directory", path.display()))?;
    Ok(parent)
}

/// The readiness line the boot oracle greps. Where it WRITES is a parameter,
/// the shape `term_selftest` and `probe_to` already use: a helper returning a
/// String leaves the emit untested, so the bytes could be pinned here and a
/// different line reach the console.
fn announce(out: &mut impl Write, path: &Path) -> Result<(), String> {
    writeln!(out, "TD-WAYLAND-READY socket={}", path.display())
        .map_err(|e| format!("write Wayland ready marker: {e}"))?;
    out.flush()
        .map_err(|e| format!("flush Wayland ready marker: {e}"))
}

pub fn serve(path: &Path, runtime: Arc<Mutex<Runtime>>) -> Result<(), String> {
    let keymap_dir = keymap_directory(path)?.to_path_buf();
    let keymap = keymap_file(&keymap_dir)?;
    socket::remove_stale(path, "Wayland")?;
    let listener = UnixListener::bind(path)
        .map_err(|e| format!("bind Wayland socket {}: {e}", path.display()))?;
    fs::set_permissions(path, Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod Wayland socket {}: {e}", path.display()))?;
    // `writeln!` rather than `println!`, which PANICS on a write failure; and
    // `lock()` because `println!` held one for the whole write, so an unlocked
    // handle could interleave this with another thread's line and reach the
    // oracle as neither.
    announce(&mut std::io::stdout().lock(), path)?;
    for connection in listener.incoming() {
        let stream = connection.map_err(|e| format!("accept Wayland client: {e}"))?;
        let permit = match ClientPermit::acquire() {
            Ok(permit) => permit,
            Err(error) => {
                eprintln!("td-compositor: {error}");
                continue;
            }
        };
        let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
        let runtime = Arc::clone(&runtime);
        let keymap = keymap.clone();
        thread::Builder::new()
            .name(format!("wayland-client-{id}"))
            .spawn(move || {
                let _permit = permit;
                if let Err(error) = serve_client(stream, id, runtime, keymap) {
                    eprintln!("td-compositor: client {id}: {error}");
                }
            })
            .map_err(|e| format!("spawn Wayland client {id}: {e}"))?;
    }
    Ok(())
}

pub fn probe(path: &Path) -> Result<(), String> {
    UnixStream::connect(path)
        .map(|_| ())
        .map_err(|e| format!("connect Wayland socket {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    /// The bytes the boot oracle greps, taken from the EMIT rather than from
    /// a string beside it — including the newline `println!` added for free.
    #[test]
    fn the_ready_marker_is_one_line_with_its_socket() {
        let mut out = Vec::new();
        super::announce(&mut out, std::path::Path::new("/run/user/1000/wayland-0")).unwrap();
        assert_eq!(out, b"TD-WAYLAND-READY socket=/run/user/1000/wayland-0\n");
    }

    use super::*;
    use crate::bar::BAR_HEIGHT;
    use crate::framebuffer::Framebuffer;
    use crate::keyboard::{KeyInput, KeyState, ModifierState, RoutedKeyboardEvent, MOD_LOGO};
    use crate::layout::{Command, Direction, Layout};
    use crate::pointer::{PointerButtonInput, PointerButtonState, PointerScroll, PointerTarget};
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn test_directory(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "td-{label}-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    fn test_keymap() -> KeymapFile {
        let directory = test_directory("shared-keymap");
        let keymap = keymap_file(&directory).unwrap();
        fs::remove_dir(directory).unwrap();
        keymap
    }

    fn send(stream: &mut UnixStream, object: u32, opcode: u16, builder: wire::Builder) {
        stream
            .write_all(&builder.message(object, opcode).unwrap())
            .unwrap();
    }

    /// How many globals the opening burst carries. Named because the tests read
    /// that burst by COUNT: a bare number is one nothing relates to
    /// `advertise_globals`, and a global added without it makes every one of
    /// them read the next message as a global and hang waiting for it.
    /// `the_registry_advertises_exactly_the_globals_td_serves` is what ties the
    /// two together.
    const GLOBAL_COUNT: usize = 6;

    fn receive_messages(stream: &mut UnixStream, count: usize) -> Vec<wire::Message> {
        let mut bytes = Vec::new();
        let mut messages = Vec::new();
        let mut scratch = [0u8; 4096];
        while messages.len() < count {
            let received = stream.read(&mut scratch).unwrap();
            assert!(received > 0);
            bytes.extend_from_slice(scratch.get(..received).unwrap());
            while let Some(message) = wire::take(&mut bytes).unwrap() {
                messages.push(message);
            }
        }
        assert_eq!(messages.len(), count);
        messages
    }

    fn receive_configure(
        stream: &mut UnixStream,
        received: &mut Vec<u8>,
        xdg_surface: u32,
        toplevel: u32,
    ) -> ((i32, i32), u32, bool) {
        let mut scratch = [0u8; 4096];
        let mut size = None;
        let mut serial = None;
        let mut activated = false;
        while size.is_none() || serial.is_none() {
            let count = stream.read(&mut scratch).unwrap();
            assert!(count > 0);
            received.extend_from_slice(scratch.get(..count).unwrap());
            while let Some(message) = wire::take(received).unwrap() {
                if message.object == toplevel && message.opcode == 0 {
                    let mut args = wire::Cursor::new(&message.payload);
                    let width = args.i32().unwrap();
                    let height = args.i32().unwrap();
                    let state_bytes = args.u32().unwrap();
                    for _ in 0..state_bytes / 4 {
                        activated |= args.u32().unwrap() == 4;
                    }
                    args.finish().unwrap();
                    size = Some((width, height));
                } else if message.object == xdg_surface && message.opcode == 0 {
                    let mut args = wire::Cursor::new(&message.payload);
                    serial = Some(args.u32().unwrap());
                    args.finish().unwrap();
                }
            }
        }
        (
            size.unwrap_or_default(),
            serial.unwrap_or_default(),
            activated,
        )
    }

    #[test]
    fn a_first_hidden_snapshot_keeps_its_home_workspace_tile_size() {
        let key = SurfaceKey {
            client: 1,
            object: 7,
        };
        let mut layout = Layout::new();
        layout.map(key);
        layout.apply(Command::MoveToWorkspace(2));
        let snapshot = layout
            .views(320, 200, 0, 0)
            .into_iter()
            .map(|view| (view.key, view))
            .collect();
        assert_eq!(
            view_status(&snapshot, key).unwrap(),
            ViewStatus::Hidden(ToplevelState {
                width: 320,
                height: 200,
                activated: false,
                fullscreen: false,
            })
        );
    }

    #[test]
    fn seat_binding_and_keyboard_initialization_are_self_contained() {
        let stem = format!(
            "td-keyboard-protocol-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(
            &framebuffer_path,
            32,
            crate::scene::least_output_height(2),
            32 * 4,
        )
        .unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let focused = SurfaceKey {
            client: 12,
            object: 40,
        };
        runtime
            .lock()
            .unwrap()
            .commit(
                focused,
                Surface {
                    width: 1,
                    height: 1,
                    pixels: vec![1, 2, 3, 0],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        runtime
            .lock()
            .unwrap()
            .key(KeyInput {
                time: 1,
                key: 125,
                state: KeyState::Pressed,
            })
            .unwrap();
        runtime
            .lock()
            .unwrap()
            .modifiers(ModifierState {
                depressed: MOD_LOGO,
                ..ModifierState::default()
            })
            .unwrap();
        let pointer_origin = runtime
            .lock()
            .unwrap()
            .layout_snapshot()
            .get(&focused)
            .map(|view| (view.rect.x, view.rect.y))
            .unwrap();
        runtime
            .lock()
            .unwrap()
            .pointer_frame(
                2,
                i32::try_from(pointer_origin.0).unwrap(),
                i32::try_from(pointer_origin.1).unwrap(),
                &[],
                PointerScroll::default(),
            )
            .unwrap();

        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut client = Client::new(12, server, runtime, test_keymap()).unwrap();
        client.advertise_globals(2).unwrap();
        let globals = receive_messages(&mut peer, GLOBAL_COUNT);
        let seat = globals.last().unwrap();
        let mut seat_global = wire::Cursor::new(&seat.payload);
        assert_eq!(seat_global.u32().unwrap(), GLOBAL_SEAT);
        assert_eq!(seat_global.string().unwrap(), "wl_seat");
        assert_eq!(seat_global.u32().unwrap(), 7);
        seat_global.finish().unwrap();

        client.bind_global(GLOBAL_SEAT, "wl_seat", 7, 5).unwrap();
        let seat_events = receive_messages(&mut peer, 2);
        let mut capabilities = wire::Cursor::new(&seat_events.first().unwrap().payload);
        assert_eq!(capabilities.u32().unwrap(), 3);
        capabilities.finish().unwrap();
        let mut name = wire::Cursor::new(&seat_events.get(1).unwrap().payload);
        assert_eq!(name.string().unwrap(), "td-seat0");
        name.finish().unwrap();

        let mut get_keyboard = wire::Builder::new();
        get_keyboard.u32(6);
        assert!(!client.keyboard_active.load(Ordering::Acquire));
        client
            .dispatch(request(5, 1, get_keyboard).unwrap(), &mut VecDeque::new())
            .unwrap();
        assert!(client.keyboard_active.load(Ordering::Acquire));
        let mut bytes = Vec::new();
        let mut messages = Vec::new();
        let mut descriptors = Vec::new();
        let mut scratch = [0u8; 16 * 1024];
        while messages.len() < 4 {
            let received = sys::recv_with_fds(&peer, &mut scratch).unwrap();
            assert!(received.count > 0);
            bytes.extend_from_slice(scratch.get(..received.count).unwrap());
            descriptors.extend(received.fds);
            while let Some(message) = wire::take(&mut bytes).unwrap() {
                messages.push(message);
            }
        }
        assert_eq!(messages.len(), 4);
        assert_eq!(descriptors.len(), 1);

        let keymap = messages.first().unwrap();
        assert_eq!((keymap.object, keymap.opcode), (6, 0));
        let mut keymap_args = wire::Cursor::new(&keymap.payload);
        assert_eq!(keymap_args.u32().unwrap(), 1);
        let size = keymap_args.u32().unwrap();
        keymap_args.finish().unwrap();
        let file = sys::duplicate_received(*descriptors.first().unwrap()).unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        let mut keymap_bytes = vec![0; usize::try_from(size).unwrap()];
        file.read_exact_at(&mut keymap_bytes, 0).unwrap();
        assert_eq!(keymap_bytes.last(), Some(&0));
        assert_eq!(
            keymap_bytes.get(..keymap_bytes.len().saturating_sub(1)),
            Some(XKB_KEYMAP.as_bytes())
        );

        assert_eq!(
            (
                messages.get(1).unwrap().object,
                messages.get(1).unwrap().opcode
            ),
            (6, 5)
        );
        let mut enter = wire::Cursor::new(&messages.get(2).unwrap().payload);
        assert_ne!(enter.u32().unwrap(), 0);
        assert_eq!(enter.u32().unwrap(), focused.object);
        assert_eq!(enter.u32().unwrap(), 4);
        assert_eq!(enter.u32().unwrap(), 125);
        enter.finish().unwrap();
        let mut modifiers = wire::Cursor::new(&messages.get(3).unwrap().payload);
        assert_ne!(modifiers.u32().unwrap(), 0);
        assert_eq!(modifiers.u32().unwrap(), MOD_LOGO);
        assert_eq!(modifiers.u32().unwrap(), 0);
        assert_eq!(modifiers.u32().unwrap(), 0);
        assert_eq!(modifiers.u32().unwrap(), 0);
        modifiers.finish().unwrap();

        let mut pointer = wire::Builder::new();
        pointer.u32(9);
        assert!(!client.pointer_active.load(Ordering::Acquire));
        client
            .dispatch(request(5, 0, pointer).unwrap(), &mut VecDeque::new())
            .unwrap();
        assert!(client.pointer_active.load(Ordering::Acquire));
        let pointer_events = receive_messages(&mut peer, 2);
        let mut pointer_enter = wire::Cursor::new(&pointer_events.first().unwrap().payload);
        assert_ne!(pointer_enter.u32().unwrap(), 0);
        assert_eq!(pointer_enter.u32().unwrap(), focused.object);
        assert_eq!(pointer_enter.i32().unwrap(), 0);
        assert_eq!(pointer_enter.i32().unwrap(), 0);
        pointer_enter.finish().unwrap();
        assert_eq!(
            (
                pointer_events.get(1).unwrap().object,
                pointer_events.get(1).unwrap().opcode
            ),
            (9, 5)
        );
        client
            .dispatch(
                request(9, 1, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(!client.pointer_active.load(Ordering::Acquire));
        assert!(!client.pointers.lock().unwrap().contains_key(&9));
        let pointer_deleted = receive_messages(&mut peer, 1);
        let mut pointer_deleted = wire::Cursor::new(&pointer_deleted.first().unwrap().payload);
        assert_eq!(pointer_deleted.u32().unwrap(), 9);
        pointer_deleted.finish().unwrap();
        client
            .dispatch(
                request(6, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(!client.keyboard_active.load(Ordering::Acquire));
        assert!(!client.keyboards.lock().unwrap().contains_key(&6));
        let deleted = receive_messages(&mut peer, 1);
        let mut deleted_id = wire::Cursor::new(&deleted.first().unwrap().payload);
        assert_eq!(deleted_id.u32().unwrap(), 6);
        deleted_id.finish().unwrap();
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn seat_versions_gate_names_repeat_and_release() {
        let stem = format!(
            "td-keyboard-version-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 16, 16, 16 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut client = Client::new(13, server, runtime, test_keymap()).unwrap();
        client.bind_global(GLOBAL_SEAT, "wl_seat", 1, 5).unwrap();
        let seat_events = receive_messages(&mut peer, 1);
        assert_eq!(
            (
                seat_events.first().unwrap().object,
                seat_events.first().unwrap().opcode
            ),
            (5, 0)
        );

        let mut get_keyboard = wire::Builder::new();
        get_keyboard.u32(6);
        client
            .dispatch(request(5, 1, get_keyboard).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut scratch = [0u8; 4096];
        let received = sys::recv_with_fds(&peer, &mut scratch).unwrap();
        assert_eq!(received.fds.len(), 1);
        let mut bytes = scratch.get(..received.count).unwrap().to_vec();
        let keymap = wire::take(&mut bytes).unwrap().unwrap();
        assert_eq!((keymap.object, keymap.opcode), (6, 0));
        assert!(bytes.is_empty());
        sys::discard_received(&received.fds);

        assert!(client
            .dispatch(
                request(6, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap_err()
            .contains("unsupported wl_keyboard"));
        let mut get_pointer = wire::Builder::new();
        get_pointer.u32(8);
        client
            .dispatch(request(5, 0, get_pointer).unwrap(), &mut VecDeque::new())
            .unwrap();
        assert!(client
            .dispatch(
                request(8, 1, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap_err()
            .contains("unsupported wl_pointer"));
        let mut touch = wire::Builder::new();
        touch.u32(9);
        assert!(client
            .dispatch(request(5, 2, touch).unwrap(), &mut VecDeque::new())
            .unwrap_err()
            .contains("wl_touch"));
        assert_eq!(client.protocol_error_code, WL_SEAT_ERROR_MISSING_CAPABILITY);
        assert!(client
            .dispatch(
                request(5, 3, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap_err()
            .contains("unsupported wl_seat"));

        client.bind_global(GLOBAL_SEAT, "wl_seat", 5, 7).unwrap();
        receive_messages(&mut peer, 2);
        let mut get_pointer = wire::Builder::new();
        get_pointer.u32(10);
        client
            .dispatch(request(7, 0, get_pointer).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(10, 1, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        let pointer_deleted = receive_messages(&mut peer, 1);
        let mut pointer_deleted = wire::Cursor::new(&pointer_deleted.first().unwrap().payload);
        assert_eq!(pointer_deleted.u32().unwrap(), 10);
        pointer_deleted.finish().unwrap();
        client
            .dispatch(
                request(7, 3, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        let deleted = receive_messages(&mut peer, 1);
        let mut args = wire::Cursor::new(&deleted.first().unwrap().payload);
        assert_eq!(args.u32().unwrap(), 7);
        args.finish().unwrap();
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn missing_touch_capability_uses_the_wl_seat_error_code() {
        let stem = format!(
            "td-seat-error-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 8, 8, 8 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut client = Client::new(14, server, runtime, test_keymap()).unwrap();
        client.bind_global(GLOBAL_SEAT, "wl_seat", 7, 5).unwrap();
        receive_messages(&mut peer, 2);

        let mut touch = wire::Builder::new();
        touch.u32(6);
        let mut bytes = touch.message(5, 2).unwrap();
        let error = dispatch_buffered(&mut client, &mut bytes, &mut VecDeque::new()).unwrap_err();
        assert!(error.contains("wl_touch"));
        let events = receive_messages(&mut peer, 1);
        assert_eq!(
            (
                events.first().unwrap().object,
                events.first().unwrap().opcode
            ),
            (1, 0)
        );
        let mut error = wire::Cursor::new(&events.first().unwrap().payload);
        assert_eq!(error.u32().unwrap(), 5);
        assert_eq!(error.u32().unwrap(), WL_SEAT_ERROR_MISSING_CAPABILITY);
        assert!(error.string().unwrap().contains("wl_touch"));
        error.finish().unwrap();

        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn threaded_server_delivers_and_tears_down_bound_keyboard_and_pointer() {
        let stem = format!(
            "td-keyboard-threaded-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(
            &framebuffer_path,
            80,
            crate::scene::least_output_height(8),
            80 * 4,
        )
        .unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let thread_runtime = Arc::clone(&runtime);
        let worker = thread::spawn(move || serve_client(server, 91, thread_runtime, test_keymap()));

        let mut get_registry = wire::Builder::new();
        get_registry.u32(2);
        send(&mut peer, 1, 1, get_registry);
        receive_messages(&mut peer, GLOBAL_COUNT);

        let mut bind_compositor = wire::Builder::new();
        bind_compositor.u32(GLOBAL_COMPOSITOR);
        bind_compositor.string("wl_compositor").unwrap();
        bind_compositor.u32(4);
        bind_compositor.u32(3);
        send(&mut peer, 2, 0, bind_compositor);
        let mut create_surface = wire::Builder::new();
        create_surface.u32(4);
        send(&mut peer, 3, 0, create_surface);

        let mut bind_seat = wire::Builder::new();
        bind_seat.u32(GLOBAL_SEAT);
        bind_seat.string("wl_seat").unwrap();
        bind_seat.u32(7);
        bind_seat.u32(5);
        send(&mut peer, 2, 0, bind_seat);
        receive_messages(&mut peer, 2);

        let mut get_keyboard = wire::Builder::new();
        get_keyboard.u32(6);
        send(&mut peer, 5, 1, get_keyboard);
        let mut bytes = Vec::new();
        let mut initial = Vec::new();
        let mut descriptors = Vec::new();
        let mut scratch = [0u8; 4096];
        while initial.len() < 2 {
            let received = sys::recv_with_fds(&peer, &mut scratch).unwrap();
            assert!(received.count > 0);
            bytes.extend_from_slice(scratch.get(..received.count).unwrap());
            descriptors.extend(received.fds);
            while let Some(message) = wire::take(&mut bytes).unwrap() {
                initial.push(message);
            }
        }
        assert_eq!(
            initial
                .iter()
                .map(|message| (message.object, message.opcode))
                .collect::<Vec<_>>(),
            [(6, 0), (6, 5)]
        );
        assert_eq!(descriptors.len(), 1);
        sys::discard_received(&descriptors);

        let mut get_pointer = wire::Builder::new();
        get_pointer.u32(7);
        send(&mut peer, 5, 0, get_pointer);

        let surface = SurfaceKey {
            client: 91,
            object: 4,
        };
        runtime
            .lock()
            .unwrap()
            .commit(
                surface,
                Surface {
                    width: 1,
                    height: 1,
                    pixels: vec![1, 2, 3, 0],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        let focus = receive_messages(&mut peer, 2);
        assert_eq!(
            focus
                .iter()
                .map(|message| (message.object, message.opcode))
                .collect::<Vec<_>>(),
            [(6, 1), (6, 4)]
        );
        let mut enter = wire::Cursor::new(&focus.first().unwrap().payload);
        assert_ne!(enter.u32().unwrap(), 0);
        assert_eq!(enter.u32().unwrap(), surface.object);
        assert_eq!(enter.u32().unwrap(), 0);
        enter.finish().unwrap();

        let rect = runtime
            .lock()
            .unwrap()
            .layout_snapshot()
            .get(&surface)
            .unwrap()
            .rect;
        runtime
            .lock()
            .unwrap()
            .pointer_frame(
                72,
                i32::try_from(rect.x).unwrap(),
                i32::try_from(rect.y).unwrap(),
                &[],
                PointerScroll::default(),
            )
            .unwrap();
        let pointer = receive_messages(&mut peer, 2);
        assert_eq!(
            pointer
                .iter()
                .map(|message| (message.object, message.opcode))
                .collect::<Vec<_>>(),
            [(7, 0), (7, 5)]
        );
        let mut enter = wire::Cursor::new(&pointer.first().unwrap().payload);
        assert_ne!(enter.u32().unwrap(), 0);
        assert_eq!(enter.u32().unwrap(), surface.object);
        assert_eq!(enter.i32().unwrap(), 0);
        assert_eq!(enter.i32().unwrap(), 0);
        enter.finish().unwrap();

        runtime
            .lock()
            .unwrap()
            .pointer_frame(
                73,
                0,
                0,
                &[PointerButtonInput {
                    time: 73,
                    button: 272,
                    state: PointerButtonState::Pressed,
                }],
                PointerScroll::default(),
            )
            .unwrap();
        let pointer = receive_messages(&mut peer, 2);
        assert_eq!(
            pointer
                .iter()
                .map(|message| (message.object, message.opcode))
                .collect::<Vec<_>>(),
            [(7, 3), (7, 5)]
        );
        let mut button = wire::Cursor::new(&pointer.first().unwrap().payload);
        assert_ne!(button.u32().unwrap(), 0);
        assert_eq!(button.u32().unwrap(), 73);
        assert_eq!(button.u32().unwrap(), 272);
        assert_eq!(button.u32().unwrap(), 1);
        button.finish().unwrap();

        send(&mut peer, 7, 1, wire::Builder::new());
        let deleted = receive_messages(&mut peer, 1);
        let mut deleted = wire::Cursor::new(&deleted.first().unwrap().payload);
        assert_eq!(deleted.u32().unwrap(), 7);
        deleted.finish().unwrap();

        runtime
            .lock()
            .unwrap()
            .key(KeyInput {
                time: 73,
                key: 30,
                state: KeyState::Pressed,
            })
            .unwrap();
        let key = receive_messages(&mut peer, 1);
        assert_eq!(
            (key.first().unwrap().object, key.first().unwrap().opcode),
            (6, 3)
        );
        let mut args = wire::Cursor::new(&key.first().unwrap().payload);
        assert_ne!(args.u32().unwrap(), 0);
        assert_eq!(args.u32().unwrap(), 73);
        assert_eq!(args.u32().unwrap(), 30);
        assert_eq!(args.u32().unwrap(), 1);
        args.finish().unwrap();

        peer.shutdown(Shutdown::Both).unwrap();
        assert!(worker.join().unwrap().is_ok());
        assert!(runtime.lock().unwrap().keyboard_snapshot().focus.is_none());
        assert!(runtime.lock().unwrap().pointer_snapshot().focus.is_none());
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn duplicate_keyboard_subscription_releases_the_runtime_lock() {
        let stem = format!(
            "td-keyboard-subscribe-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 8, 8, 8 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let subscription = runtime.lock().unwrap().subscribe_keyboard(92).unwrap();
        let (server, _peer) = UnixStream::pair().unwrap();
        let error = serve_client(server, 92, Arc::clone(&runtime), test_keymap()).unwrap_err();
        assert!(error.contains("keyboard subscriber 92 already exists"));

        let (_, stop) = subscription.split();
        stop.stop();
        runtime.lock().unwrap().unsubscribe_keyboard(92);
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn keymap_file_is_unlinked_mode_0600_and_read_only() {
        let directory = test_directory("keymap-file-test");
        let keymap = keymap_file(&directory).unwrap();
        assert_eq!(
            usize::try_from(keymap.size).unwrap(),
            XKB_KEYMAP.len().saturating_add(1)
        );
        assert_eq!(
            keymap.file.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(keymap.file.write_all_at(b"x", 0).is_err());
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        let second = keymap.clone();
        assert!(Arc::ptr_eq(&keymap.file, &second.file));
        drop(keymap);
        drop(second);
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn keymap_directory_rejects_a_relative_socket_without_a_parent() {
        assert!(keymap_directory(Path::new("wayland-0")).is_err());
        assert_eq!(
            keymap_directory(Path::new("/run/user/1000/wayland-0")).unwrap(),
            Path::new("/run/user/1000")
        );
    }

    #[test]
    fn descriptor_send_treats_peer_departure_as_a_clean_disconnect() {
        let (server, peer) = UnixStream::pair().unwrap();
        drop(peer);
        let directory = test_directory("descriptor-disconnect-test");
        let keymap = keymap_file(&directory).unwrap();
        fs::remove_dir(directory).unwrap();
        let mut outbound = Outbound {
            stream: server,
            disconnected: false,
        };
        assert!(outbound
            .send_with_fd(b"event", keymap.file.as_raw_fd())
            .is_ok());
        assert!(outbound.disconnected);
    }

    #[test]
    fn surface_delete_id_follows_its_queued_keyboard_leave() {
        let stem = format!(
            "td-keyboard-surface-barrier-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 8, 8, 8 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let subscription = runtime.lock().unwrap().subscribe_keyboard(88).unwrap();
        let (receiver, stop) = subscription.split();
        let surface = SurfaceKey {
            client: 88,
            object: 7,
        };
        runtime
            .lock()
            .unwrap()
            .commit(
                surface,
                Surface {
                    width: 1,
                    height: 1,
                    pixels: vec![1, 2, 3, 0],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        let revision = runtime.lock().unwrap().keyboard_snapshot().revision;

        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut client = Client::new(88, server, Arc::clone(&runtime), test_keymap()).unwrap();
        client
            .objects
            .insert(surface.object, Object::Surface(SurfaceState::default()));
        client.keyboards.lock().unwrap().insert(
            9,
            KeyboardRegistration {
                after_revision: revision,
            },
        );
        let worker_stop = stop.clone();
        let worker_keyboards = Arc::clone(&client.keyboards);
        let worker_pending_deletes = Arc::clone(&client.pending_deletes);
        let worker_outbound = Arc::clone(&client.outbound);
        let worker = thread::spawn(move || {
            seat_worker(
                receiver,
                worker_stop,
                worker_keyboards,
                Arc::new(Mutex::new(BTreeMap::new())),
                Arc::new(Mutex::new(None)),
                worker_pending_deletes,
                worker_outbound,
            )
        });

        client
            .dispatch(
                request(surface.object, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        let events = receive_messages(&mut peer, 2);
        assert_eq!(
            events
                .iter()
                .map(|event| (event.object, event.opcode))
                .collect::<Vec<_>>(),
            [(9, 2), (1, 1)]
        );
        let mut leave = wire::Cursor::new(&events.first().unwrap().payload);
        leave.u32().unwrap();
        assert_eq!(leave.u32().unwrap(), surface.object);
        leave.finish().unwrap();
        let mut deleted = wire::Cursor::new(&events.get(1).unwrap().payload);
        assert_eq!(deleted.u32().unwrap(), surface.object);
        deleted.finish().unwrap();
        assert!(client
            .insert(surface.object, Object::Region(Arc::new(InputRegion::new())),)
            .is_ok());

        stop.stop();
        runtime.lock().unwrap().unsubscribe_keyboard(88);
        assert!(worker.join().unwrap().is_ok());
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn surface_delete_id_follows_its_queued_modal_grab_leave_and_frame() {
        let stem = format!(
            "td-pointer-surface-barrier-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(
            &framebuffer_path,
            80,
            crate::scene::least_output_height(8),
            80 * 4,
        )
        .unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut client = Client::new(88, server, Arc::clone(&runtime), test_keymap()).unwrap();
        let subscription = runtime
            .lock()
            .unwrap()
            .subscribe_input_with_activity(
                88,
                Arc::clone(&client.keyboard_active),
                Arc::clone(&client.pointer_active),
            )
            .unwrap();
        let (receiver, stop) = subscription.split();
        let surface = SurfaceKey {
            client: 88,
            object: 7,
        };
        runtime
            .lock()
            .unwrap()
            .commit(
                surface,
                Surface {
                    width: 32,
                    height: 32,
                    pixels: vec![1; 32 * 32 * 4],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        let rect = runtime
            .lock()
            .unwrap()
            .layout_snapshot()
            .get(&surface)
            .unwrap()
            .rect;
        runtime
            .lock()
            .unwrap()
            .pointer_frame(
                1,
                i32::try_from(rect.x).unwrap(),
                i32::try_from(rect.y).unwrap(),
                &[],
                PointerScroll::default(),
            )
            .unwrap();
        let press = PointerButtonInput {
            time: 2,
            button: 272,
            state: PointerButtonState::Pressed,
        };
        runtime
            .lock()
            .unwrap()
            .pointer_frame(2, 0, 0, &[press], PointerScroll::default())
            .unwrap();
        runtime
            .lock()
            .unwrap()
            .launcher(crate::launcher::LauncherAction::Open)
            .unwrap();
        let revision = runtime.lock().unwrap().pointer_snapshot().revision;
        client
            .objects
            .insert(surface.object, Object::Surface(SurfaceState::default()));
        client.pointers.lock().unwrap().insert(
            9,
            PointerRegistration {
                after_revision: revision,
                version: 7,
            },
        );
        client.pointer_active.store(true, Ordering::Release);
        let worker_stop = stop.clone();
        let worker_pointers = Arc::clone(&client.pointers);
        let worker_authority = Arc::clone(&client.pointer_authority);
        let worker_pending_deletes = Arc::clone(&client.pending_deletes);
        let worker_outbound = Arc::clone(&client.outbound);
        let worker = thread::spawn(move || {
            seat_worker(
                receiver,
                worker_stop,
                Arc::new(Mutex::new(BTreeMap::new())),
                worker_pointers,
                worker_authority,
                worker_pending_deletes,
                worker_outbound,
            )
        });

        client
            .dispatch(
                request(surface.object, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        let events = receive_messages(&mut peer, 3);
        assert_eq!(
            events
                .iter()
                .map(|event| (event.object, event.opcode))
                .collect::<Vec<_>>(),
            [(9, 1), (9, 5), (1, 1)]
        );
        let mut leave = wire::Cursor::new(&events.first().unwrap().payload);
        leave.u32().unwrap();
        assert_eq!(leave.u32().unwrap(), surface.object);
        leave.finish().unwrap();
        let mut deleted = wire::Cursor::new(&events.get(2).unwrap().payload);
        assert_eq!(deleted.u32().unwrap(), surface.object);
        deleted.finish().unwrap();
        assert!(client
            .insert(surface.object, Object::Region(Arc::new(InputRegion::new())),)
            .is_ok());

        stop.stop();
        runtime.lock().unwrap().unsubscribe_keyboard(88);
        assert!(worker.join().unwrap().is_ok());
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn surface_destroy_does_not_wait_for_the_seat_worker() {
        let stem = format!(
            "td-keyboard-surface-nonblocking-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 8, 8, 8 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let subscription = runtime.lock().unwrap().subscribe_keyboard(89).unwrap();
        let (receiver, stop) = subscription.split();
        let surface = SurfaceKey {
            client: 89,
            object: 7,
        };
        runtime
            .lock()
            .unwrap()
            .commit(
                surface,
                Surface {
                    width: 1,
                    height: 1,
                    pixels: vec![1, 2, 3, 0],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();

        let (server, _peer) = UnixStream::pair().unwrap();
        let mut client = Client::new(89, server, Arc::clone(&runtime), test_keymap()).unwrap();
        client
            .objects
            .insert(surface.object, Object::Surface(SurfaceState::default()));
        client
            .dispatch(
                request(surface.object, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        let reservation = client
            .pending_deletes
            .lock()
            .unwrap()
            .get(&surface.object)
            .cloned()
            .unwrap();
        let blocked_delete = reservation.lock().unwrap();
        assert!(client
            .insert(8, Object::Region(Arc::new(InputRegion::new())))
            .is_ok());
        drop(blocked_delete);
        assert!(client
            .insert(surface.object, Object::Region(Arc::new(InputRegion::new())),)
            .unwrap_err()
            .contains("before delete_id"));

        let deliveries: Vec<KeyboardDelivery> = (0..4).map(|_| receiver.recv().unwrap()).collect();
        assert!(matches!(
            deliveries.get(2),
            Some(KeyboardDelivery::Event(RoutedKeyboardEvent {
                event: KeyboardEvent::Leave { surface: left },
                ..
            })) if *left == surface
        ));
        assert!(matches!(
            deliveries.get(3),
            Some(KeyboardDelivery::DeleteId(7))
        ));

        stop.stop();
        runtime.lock().unwrap().unsubscribe_keyboard(89);
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn surface_delete_without_a_worker_clears_its_id_reservation() {
        let stem = format!(
            "td-keyboard-surface-direct-delete-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 8, 8, 8 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut client = Client::new(90, server, runtime, test_keymap()).unwrap();
        client
            .objects
            .insert(7, Object::Surface(SurfaceState::default()));

        client
            .dispatch(
                request(7, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        let events = receive_messages(&mut peer, 1);
        assert_eq!(
            (
                events.first().unwrap().object,
                events.first().unwrap().opcode
            ),
            (1, 1)
        );
        assert!(client
            .insert(7, Object::Region(Arc::new(InputRegion::new())))
            .is_ok());
        assert!(client.pending_deletes.lock().unwrap().is_empty());

        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn seat_worker_filters_keyboard_history_and_preserves_order() {
        let stem = format!(
            "td-keyboard-worker-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 32, 32, 32 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime.subscribe_keyboard(88).unwrap();
        let (receiver, stop) = subscription.split();
        let first = SurfaceKey {
            client: 88,
            object: 7,
        };
        runtime
            .commit(
                first,
                Surface {
                    width: 1,
                    height: 1,
                    pixels: vec![1, 2, 3, 0],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        let snapshot = runtime.keyboard_snapshot();
        let mut registrations = BTreeMap::new();
        registrations.insert(
            9,
            KeyboardRegistration {
                after_revision: snapshot.revision,
            },
        );
        registrations.insert(
            10,
            KeyboardRegistration {
                after_revision: snapshot.revision,
            },
        );
        let registrations = Arc::new(Mutex::new(registrations));
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let outbound = Arc::new(Mutex::new(Outbound {
            stream: server,
            disconnected: false,
        }));
        let worker_stop = stop.clone();
        let worker_registrations = Arc::clone(&registrations);
        let worker_outbound = Arc::clone(&outbound);
        let worker = thread::spawn(move || {
            seat_worker(
                receiver,
                worker_stop,
                worker_registrations,
                Arc::new(Mutex::new(BTreeMap::new())),
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(BTreeMap::new())),
                worker_outbound,
            )
        });

        runtime
            .key(KeyInput {
                time: 54,
                key: 30,
                state: KeyState::Pressed,
            })
            .unwrap();
        let key_events = receive_messages(&mut peer, 2);
        let key_event = key_events.first().unwrap();
        assert_eq!((key_event.object, key_event.opcode), (9, 3));
        let mut key = wire::Cursor::new(&key_event.payload);
        let serial = key.u32().unwrap();
        assert_ne!(serial, 0);
        assert_eq!(key.u32().unwrap(), 54);
        assert_eq!(key.u32().unwrap(), 30);
        assert_eq!(key.u32().unwrap(), 1);
        key.finish().unwrap();
        let second_key_event = key_events.get(1).unwrap();
        assert_eq!((second_key_event.object, second_key_event.opcode), (10, 3));
        let mut second_key = wire::Cursor::new(&second_key_event.payload);
        assert_eq!(second_key.u32().unwrap(), serial);
        assert_eq!(second_key.u32().unwrap(), 54);
        assert_eq!(second_key.u32().unwrap(), 30);
        assert_eq!(second_key.u32().unwrap(), 1);
        second_key.finish().unwrap();

        let second = SurfaceKey {
            client: 88,
            object: 8,
        };
        runtime
            .commit(
                second,
                Surface {
                    width: 1,
                    height: 1,
                    pixels: vec![4, 5, 6, 0],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        let focus = receive_messages(&mut peer, 6);
        assert_eq!(
            focus
                .iter()
                .map(|event| (event.object, event.opcode))
                .collect::<Vec<_>>(),
            [(9, 2), (10, 2), (9, 1), (10, 1), (9, 4), (10, 4)]
        );
        let mut leave = wire::Cursor::new(&focus.first().unwrap().payload);
        let leave_serial = leave.u32().unwrap();
        assert_eq!(leave.u32().unwrap(), first.object);
        leave.finish().unwrap();
        let mut second_leave = wire::Cursor::new(&focus.get(1).unwrap().payload);
        assert_eq!(second_leave.u32().unwrap(), leave_serial);
        assert_eq!(second_leave.u32().unwrap(), first.object);
        second_leave.finish().unwrap();
        let mut enter = wire::Cursor::new(&focus.get(2).unwrap().payload);
        let enter_serial = enter.u32().unwrap();
        assert_eq!(enter.u32().unwrap(), second.object);
        assert_eq!(enter.u32().unwrap(), 4);
        assert_eq!(enter.u32().unwrap(), 30);
        enter.finish().unwrap();
        let mut second_enter = wire::Cursor::new(&focus.get(3).unwrap().payload);
        assert_eq!(second_enter.u32().unwrap(), enter_serial);
        assert_eq!(second_enter.u32().unwrap(), second.object);
        assert_eq!(second_enter.u32().unwrap(), 4);
        assert_eq!(second_enter.u32().unwrap(), 30);
        second_enter.finish().unwrap();

        stop.stop();
        runtime.unsubscribe_keyboard(88);
        assert!(worker.join().unwrap().is_ok());
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn pointer_worker_encodes_frames_versions_and_shared_serials() {
        let stem = format!(
            "td-pointer-worker-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(
            &framebuffer_path,
            80,
            crate::scene::least_output_height(8),
            80 * 4,
        )
        .unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let pointer_active = Arc::new(AtomicBool::new(true));
        let subscription = runtime
            .subscribe_input_with_activity(88, Arc::new(AtomicBool::new(false)), pointer_active)
            .unwrap();
        let (receiver, stop) = subscription.split();
        let surface = SurfaceKey {
            client: 88,
            object: 7,
        };
        runtime
            .commit(
                surface,
                Surface {
                    width: 32,
                    height: 32,
                    pixels: vec![1; 32 * 32 * 4],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        let rect = runtime.layout_snapshot().get(&surface).unwrap().rect;
        let pointers = Arc::new(Mutex::new(BTreeMap::from([
            (
                9,
                PointerRegistration {
                    after_revision: 0,
                    version: 7,
                },
            ),
            (
                10,
                PointerRegistration {
                    after_revision: 0,
                    version: 4,
                },
            ),
        ])));
        let pointer_authority = Arc::new(Mutex::new(None));
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let outbound = Arc::new(Mutex::new(Outbound {
            stream: server,
            disconnected: false,
        }));
        let worker_stop = stop.clone();
        let worker_pointers = Arc::clone(&pointers);
        let worker_authority = Arc::clone(&pointer_authority);
        let worker_outbound = Arc::clone(&outbound);
        let worker = thread::spawn(move || {
            seat_worker(
                receiver,
                worker_stop,
                Arc::new(Mutex::new(BTreeMap::new())),
                worker_pointers,
                worker_authority,
                Arc::new(Mutex::new(BTreeMap::new())),
                worker_outbound,
            )
        });

        runtime
            .pointer_frame(
                40,
                i32::try_from(rect.x.saturating_add(2)).unwrap(),
                i32::try_from(rect.y.saturating_add(3)).unwrap(),
                &[],
                PointerScroll::default(),
            )
            .unwrap();
        let enter = receive_messages(&mut peer, 3);
        assert_eq!(
            enter
                .iter()
                .map(|event| (event.object, event.opcode))
                .collect::<Vec<_>>(),
            [(9, 0), (9, 5), (10, 0)]
        );
        let mut first = wire::Cursor::new(&enter.first().unwrap().payload);
        let serial = first.u32().unwrap();
        assert_ne!(serial, 0);
        assert_eq!(first.u32().unwrap(), surface.object);
        assert_eq!(first.i32().unwrap(), 2 * 256);
        assert_eq!(first.i32().unwrap(), 3 * 256);
        first.finish().unwrap();
        let mut second = wire::Cursor::new(&enter.get(2).unwrap().payload);
        assert_eq!(second.u32().unwrap(), serial);
        assert_eq!(second.u32().unwrap(), surface.object);
        assert_eq!(second.i32().unwrap(), 2 * 256);
        assert_eq!(second.i32().unwrap(), 3 * 256);
        second.finish().unwrap();
        assert_eq!(
            *pointer_authority.lock().unwrap(),
            Some(PointerEnterAuthority {
                serial,
                surface: surface.object,
                after_revision: 1,
            })
        );

        runtime
            .pointer_frame(41, 1, 2, &[], PointerScroll::default())
            .unwrap();
        let motion = receive_messages(&mut peer, 3);
        assert_eq!(
            motion
                .iter()
                .map(|event| (event.object, event.opcode))
                .collect::<Vec<_>>(),
            [(9, 2), (9, 5), (10, 2)]
        );
        let mut first = wire::Cursor::new(&motion.first().unwrap().payload);
        assert_eq!(first.u32().unwrap(), 41);
        assert_eq!(first.i32().unwrap(), 3 * 256);
        assert_eq!(first.i32().unwrap(), 5 * 256);
        first.finish().unwrap();

        let button = PointerButtonInput {
            time: 42,
            button: 272,
            state: PointerButtonState::Pressed,
        };
        runtime
            .pointer_frame(42, 0, 0, &[button], PointerScroll::default())
            .unwrap();
        let buttons = receive_messages(&mut peer, 3);
        assert_eq!(
            buttons
                .iter()
                .map(|event| (event.object, event.opcode))
                .collect::<Vec<_>>(),
            [(9, 3), (9, 5), (10, 3)]
        );
        let mut first = wire::Cursor::new(&buttons.first().unwrap().payload);
        let serial = first.u32().unwrap();
        assert_eq!(first.u32().unwrap(), 42);
        assert_eq!(first.u32().unwrap(), 272);
        assert_eq!(first.u32().unwrap(), 1);
        first.finish().unwrap();
        let mut second = wire::Cursor::new(&buttons.get(2).unwrap().payload);
        assert_eq!(second.u32().unwrap(), serial);
        assert_eq!(second.u32().unwrap(), 42);
        assert_eq!(second.u32().unwrap(), 272);
        assert_eq!(second.u32().unwrap(), 1);
        second.finish().unwrap();

        // A WHEEL over the same seat, driven from a report rather than a
        // hand-built frame: this is the only place a scroll is observed on
        // the wire from end to end, which is where the qualifiers' position
        // relative to `wl_pointer.frame` can actually be seen. The version 7
        // object gets the triple and its frame; the version 4 object gets the
        // axis alone and no frame at all.
        runtime
            .pointer_frame(
                41,
                0,
                0,
                &[],
                PointerScroll {
                    vertical: 1,
                    horizontal: 0,
                },
            )
            .unwrap();
        let scrolled = receive_messages(&mut peer, 5);
        assert_eq!(
            scrolled
                .iter()
                .map(|event| (event.object, event.opcode))
                .collect::<Vec<_>>(),
            [
                (9, WL_POINTER_AXIS_SOURCE),
                (9, WL_POINTER_AXIS_DISCRETE),
                (9, 4),
                (9, 5),
                (10, 4),
            ]
        );
        // The value is the protocol's, so a wheel pushed away from the
        // operator arrives as a downward scroll — the sign flip surviving
        // every layer between the device and the socket.
        let mut axis = wire::Cursor::new(&scrolled.get(2).unwrap().payload);
        assert_eq!(axis.u32().unwrap(), 41);
        assert_eq!(axis.u32().unwrap(), 0, "vertical");
        assert_eq!(axis.i32().unwrap(), -10 * 256);
        axis.finish().unwrap();
        // No serial: the protocol gives one to the events a client may quote
        // back as authority, and a notch is not one of those.
        let mut bare = wire::Cursor::new(&scrolled.get(4).unwrap().payload);
        assert_eq!(bare.u32().unwrap(), 41);
        assert_eq!(bare.u32().unwrap(), 0);
        assert_eq!(bare.i32().unwrap(), -10 * 256);
        bare.finish().unwrap();

        stop.stop();
        runtime.unsubscribe_keyboard(88);
        assert!(worker.join().unwrap().is_ok());
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn queued_split_transition_preserves_a_new_pointer_initial_authority() {
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let outbound = Arc::new(Mutex::new(Outbound {
            stream: server,
            disconnected: false,
        }));
        let pointers = BTreeMap::from([
            (
                9,
                PointerRegistration {
                    after_revision: 0,
                    version: 7,
                },
            ),
            (
                10,
                PointerRegistration {
                    after_revision: 7,
                    version: 7,
                },
            ),
        ]);
        let target = PointerTarget {
            surface: SurfaceKey {
                client: 88,
                object: 7,
            },
            x: 3,
            y: 5,
        };
        let leave = RoutedPointerFrame {
            revision: 6,
            client: 88,
            events: vec![PointerEvent::Leave {
                surface: SurfaceKey {
                    client: 88,
                    object: 6,
                },
            }],
        };
        let enter = RoutedPointerFrame {
            revision: 7,
            client: 88,
            events: vec![PointerEvent::Enter { target }],
        };
        let mut authority = Some(PointerEnterAuthority {
            serial: 4242,
            surface: target.surface.object,
            after_revision: 7,
        });

        send_pointer_frame(&outbound, &pointers, &mut authority, &leave).unwrap();
        let leave_events = receive_messages(&mut peer, 2);
        assert_eq!(
            leave_events
                .iter()
                .map(|event| (event.object, event.opcode))
                .collect::<Vec<_>>(),
            [(9, 1), (9, 5)]
        );
        assert_eq!(
            authority,
            Some(PointerEnterAuthority {
                serial: 4242,
                surface: target.surface.object,
                after_revision: 7,
            })
        );

        send_pointer_frame(&outbound, &pointers, &mut authority, &enter).unwrap();
        let enter_events = receive_messages(&mut peer, 2);
        assert_eq!(
            enter_events
                .iter()
                .map(|event| (event.object, event.opcode))
                .collect::<Vec<_>>(),
            [(9, 0), (9, 5)]
        );
        let mut enter_event = wire::Cursor::new(&enter_events.first().unwrap().payload);
        assert_eq!(enter_event.u32().unwrap(), 4242);
        assert_eq!(enter_event.u32().unwrap(), target.surface.object);
        assert_eq!(enter_event.i32().unwrap(), 3 * 256);
        assert_eq!(enter_event.i32().unwrap(), 5 * 256);
        enter_event.finish().unwrap();
        assert_eq!(
            authority,
            Some(PointerEnterAuthority {
                serial: 4242,
                surface: target.surface.object,
                after_revision: 7,
            })
        );
    }

    #[test]
    fn a_tilting_wheel_names_its_source_once_for_the_whole_frame() {
        // `axis_source` "carries the source information for all events within
        // that frame", so it belongs to the FRAME rather than to an axis — and
        // a tilting wheel is what tells the two apart, being two axis events
        // under one `wl_pointer.frame`. Sent per axis it would assert the same
        // source twice.
        use crate::pointer::{AxisStep, PointerAxis};
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let outbound = Arc::new(Mutex::new(Outbound {
            stream: server,
            disconnected: false,
        }));
        let pointers = BTreeMap::from([(
            9,
            PointerRegistration {
                after_revision: 0,
                version: 5,
            },
        )]);
        let surface = SurfaceKey {
            client: 88,
            object: 7,
        };
        let both = RoutedPointerFrame {
            revision: 3,
            client: 88,
            events: vec![
                PointerEvent::Axis {
                    surface,
                    time: 12,
                    step: AxisStep::of(PointerAxis::Vertical, -1),
                },
                PointerEvent::Axis {
                    surface,
                    time: 12,
                    step: AxisStep::of(PointerAxis::Horizontal, 2),
                },
            ],
        };
        let mut authority = None;
        send_pointer_frame(&outbound, &pointers, &mut authority, &both).unwrap();
        let events = receive_messages(&mut peer, 6);
        assert_eq!(
            events
                .iter()
                .map(|event| (event.object, event.opcode))
                .collect::<Vec<_>>(),
            [
                (9, WL_POINTER_AXIS_SOURCE),
                (9, WL_POINTER_AXIS_DISCRETE),
                (9, 4),
                (9, WL_POINTER_AXIS_DISCRETE),
                (9, 4),
                (9, 5),
            ],
            "the source is not once per frame"
        );

        // Each axis still names ITSELF, so the one source is not standing in
        // for a discrete event too.
        let mut first = wire::Cursor::new(&events.get(1).unwrap().payload);
        assert_eq!(first.u32().unwrap(), 0, "vertical");
        assert_eq!(first.i32().unwrap(), -1);
        first.finish().unwrap();
        let mut second = wire::Cursor::new(&events.get(3).unwrap().payload);
        assert_eq!(second.u32().unwrap(), 1, "horizontal");
        assert_eq!(second.i32().unwrap(), 2);
        second.finish().unwrap();

        // A frame with ONE axis still carries a source, or the branch above
        // would pass by never sending one at all.
        let alone = RoutedPointerFrame {
            revision: 4,
            client: 88,
            events: vec![PointerEvent::Axis {
                surface,
                time: 13,
                step: AxisStep::of(PointerAxis::Vertical, 1),
            }],
        };
        send_pointer_frame(&outbound, &pointers, &mut authority, &alone).unwrap();
        assert_eq!(
            receive_messages(&mut peer, 4)
                .iter()
                .map(|event| (event.object, event.opcode))
                .collect::<Vec<_>>(),
            [
                (9, WL_POINTER_AXIS_SOURCE),
                (9, WL_POINTER_AXIS_DISCRETE),
                (9, 4),
                (9, 5),
            ]
        );
    }

    #[test]
    fn initial_pointer_enter_gates_frame_at_version_five() {
        let target = PointerTarget {
            surface: SurfaceKey {
                client: 8,
                object: 4,
            },
            x: 3,
            y: 5,
        };
        for (version, count) in [(4, 1), (5, 2)] {
            let (server, mut peer) = UnixStream::pair().unwrap();
            let outbound = Arc::new(Mutex::new(Outbound {
                stream: server,
                disconnected: false,
            }));
            send_pointer_initial(
                &outbound,
                9,
                version,
                8,
                PointerSnapshot {
                    revision: 7,
                    focus: Some(target),
                },
                Some(33),
            )
            .unwrap();
            let events = receive_messages(&mut peer, count);
            assert_eq!(
                events.first().map(|event| (event.object, event.opcode)),
                Some((9, 0))
            );
            if version >= 5 {
                assert_eq!(
                    events.get(1).map(|event| (event.object, event.opcode)),
                    Some((9, 5))
                );
            }
        }
    }

    #[test]
    fn seat_worker_distinguishes_overflow_from_requested_stop() {
        let stem = format!(
            "td-keyboard-stop-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 8, 8, 8 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime.subscribe_keyboard(99).unwrap();
        let (receiver, stop) = subscription.split();
        runtime.unsubscribe_keyboard(99);
        let (server, _peer) = UnixStream::pair().unwrap();
        let error = seat_worker(
            receiver,
            stop,
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(Mutex::new(Outbound {
                stream: server,
                disconnected: false,
            })),
        )
        .unwrap_err();
        assert!(error.contains("before client teardown"));
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn bound_seat_worker_disconnects_after_queue_overflow() {
        let stem = format!(
            "td-keyboard-overflow-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 8, 8, 8 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime.subscribe_keyboard(99).unwrap();
        let (receiver, stop) = subscription.split();
        runtime
            .commit(
                SurfaceKey {
                    client: 99,
                    object: 7,
                },
                Surface {
                    width: 1,
                    height: 1,
                    pixels: vec![1, 2, 3, 0],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        for time in 0..=crate::runtime::MAX_PENDING_KEYBOARD_DELIVERIES {
            runtime
                .key(KeyInput {
                    time: u32::try_from(time).unwrap(),
                    key: 30,
                    state: if time % 2 == 0 {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    },
                })
                .unwrap();
        }

        let registrations = Arc::new(Mutex::new(BTreeMap::from([(
            9,
            KeyboardRegistration { after_revision: 0 },
        )])));
        let (server, _peer) = UnixStream::pair().unwrap();
        let outbound = Arc::new(Mutex::new(Outbound {
            stream: server,
            disconnected: false,
        }));
        let error = supervise_seat_worker(
            99,
            receiver,
            stop,
            registrations,
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::clone(&outbound),
        )
        .unwrap_err();
        assert!(error.contains("before client teardown"));
        assert!(outbound.lock().unwrap().disconnected);
        runtime.unsubscribe_keyboard(99);
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn stopped_seat_worker_clears_queued_delete_reservations() {
        let stem = format!(
            "td-keyboard-stop-delete-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 8, 8, 8 * 4).unwrap();
        let mut runtime = Runtime::new(framebuffer);
        let subscription = runtime.subscribe_keyboard(99).unwrap();
        let (receiver, stop) = subscription.split();
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        for id in [7, 8] {
            pending.lock().unwrap().insert(id, Arc::new(Mutex::new(())));
            assert!(runtime.queue_keyboard_delete(99, id).unwrap());
        }
        stop.stop();
        let (server, _peer) = UnixStream::pair().unwrap();
        assert!(seat_worker(
            receiver,
            stop,
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(Mutex::new(None)),
            Arc::clone(&pending),
            Arc::new(Mutex::new(Outbound {
                stream: server,
                disconnected: false,
            })),
        )
        .is_ok());
        assert!(pending.lock().unwrap().is_empty());
        runtime.unsubscribe_keyboard(99);
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn shm_commit_and_transient_remap_preserve_pixels_and_workspace() {
        let stem = format!(
            "td-wayland-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let pool_path = std::env::temp_dir().join(format!("{stem}.pool"));
        let framebuffer = Framebuffer::test_file(
            &framebuffer_path,
            64,
            crate::scene::least_output_height(2),
            64 * 4,
        )
        .unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        runtime.lock().unwrap().repaint().unwrap();
        let (server, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let thread_runtime = Arc::clone(&runtime);
        let keymap = test_keymap();
        let worker = thread::spawn(move || serve_client(server, 7, thread_runtime, keymap));

        let mut get_registry = wire::Builder::new();
        get_registry.u32(2);
        send(&mut client, 1, 1, get_registry);

        let mut bind_compositor = wire::Builder::new();
        bind_compositor.u32(GLOBAL_COMPOSITOR);
        bind_compositor.string("wl_compositor").unwrap();
        bind_compositor.u32(4);
        bind_compositor.u32(3);
        send(&mut client, 2, 0, bind_compositor);

        let mut bind_shm = wire::Builder::new();
        bind_shm.u32(GLOBAL_SHM);
        bind_shm.string("wl_shm").unwrap();
        bind_shm.u32(1);
        bind_shm.u32(4);
        send(&mut client, 2, 0, bind_shm);

        let mut create_surface = wire::Builder::new();
        create_surface.u32(5);
        send(&mut client, 3, 0, create_surface);

        let mut bind_xdg = wire::Builder::new();
        bind_xdg.u32(GLOBAL_XDG_WM_BASE);
        bind_xdg.string("xdg_wm_base").unwrap();
        bind_xdg.u32(1);
        bind_xdg.u32(8);
        send(&mut client, 2, 0, bind_xdg);

        let mut get_xdg_surface = wire::Builder::new();
        get_xdg_surface.u32(9);
        get_xdg_surface.u32(5);
        send(&mut client, 8, 2, get_xdg_surface);

        let mut get_toplevel = wire::Builder::new();
        get_toplevel.u32(10);
        send(&mut client, 9, 1, get_toplevel);
        let mut initial_detach = wire::Builder::new();
        initial_detach.u32(0);
        initial_detach.i32(0);
        initial_detach.i32(0);
        send(&mut client, 5, 1, initial_detach);
        send(&mut client, 5, 6, wire::Builder::new());

        let mut received = Vec::new();
        let mut scratch = [0u8; 4096];
        let configure = loop {
            let count = client.read(&mut scratch).unwrap();
            assert!(count > 0);
            received.extend_from_slice(scratch.get(..count).unwrap());
            let mut serial = None;
            while let Some(message) = wire::take(&mut received).unwrap() {
                if message.object == 9 && message.opcode == 0 {
                    let mut args = wire::Cursor::new(&message.payload);
                    serial = Some(args.u32().unwrap());
                    args.finish().unwrap();
                }
            }
            if let Some(serial) = serial {
                break serial;
            }
        };
        let mut ack = wire::Builder::new();
        ack.u32(configure);
        send(&mut client, 9, 4, ack);

        let mut pixels = Vec::new();
        for _ in 0..16 * 16 {
            pixels.extend_from_slice(&[0x11u8, 0x22, 0x33, 0]);
        }
        fs::write(&pool_path, &pixels).unwrap();
        let pool = File::open(&pool_path).unwrap();
        let mut create_pool = wire::Builder::new();
        create_pool.u32(6);
        create_pool.i32(i32::try_from(pixels.len()).unwrap());
        let create_pool = create_pool.message(4, 0).unwrap();
        sys::send_with_fd(&client, &create_pool, pool.as_raw_fd()).unwrap();

        let mut create_buffer = wire::Builder::new();
        create_buffer.u32(7);
        create_buffer.i32(0);
        create_buffer.i32(16);
        create_buffer.i32(16);
        create_buffer.i32(16 * 4);
        create_buffer.u32(SHM_XRGB8888);
        send(&mut client, 6, 0, create_buffer);

        let mut attach = wire::Builder::new();
        attach.u32(7);
        attach.i32(0);
        attach.i32(0);
        send(&mut client, 5, 1, attach);
        send(&mut client, 5, 6, wire::Builder::new());

        let mut saw_release = false;
        while !saw_release {
            let count = client.read(&mut scratch).unwrap();
            assert!(count > 0);
            received.extend_from_slice(scratch.get(..count).unwrap());
            while let Some(message) = wire::take(&mut received).unwrap() {
                if message.object == 7 && message.opcode == 0 {
                    saw_release = true;
                }
            }
        }
        let frame = fs::read(&framebuffer_path).unwrap();
        assert!(frame.as_chunks::<4>().0.contains(&[0x11, 0x22, 0x33, 0]));

        runtime
            .lock()
            .unwrap()
            .command(Command::MoveToWorkspace(2))
            .unwrap();
        let mut saw_hidden = false;
        let hidden_configure = loop {
            let count = client.read(&mut scratch).unwrap();
            assert!(count > 0);
            received.extend_from_slice(scratch.get(..count).unwrap());
            let mut serial = None;
            while let Some(message) = wire::take(&mut received).unwrap() {
                if message.object == 10 && message.opcode == 0 {
                    let mut args = wire::Cursor::new(&message.payload);
                    assert!(args.i32().unwrap() > 0);
                    assert!(args.i32().unwrap() > 0);
                    let state_bytes = args.u32().unwrap();
                    for _ in 0..state_bytes / 4 {
                        args.u32().unwrap();
                    }
                    saw_hidden = state_bytes == 0;
                    args.finish().unwrap();
                } else if message.object == 9 && message.opcode == 0 {
                    let mut args = wire::Cursor::new(&message.payload);
                    let candidate = args.u32().unwrap();
                    args.finish().unwrap();
                    if saw_hidden {
                        serial = Some(candidate);
                    }
                    saw_hidden = false;
                }
            }
            if let Some(serial) = serial {
                break serial;
            }
        };
        let mut ack = wire::Builder::new();
        ack.u32(hidden_configure);
        send(&mut client, 9, 4, ack);

        let mut detach = wire::Builder::new();
        detach.u32(0);
        detach.i32(0);
        detach.i32(0);
        send(&mut client, 5, 1, detach);
        send(&mut client, 5, 6, wire::Builder::new());

        let mut remap_detach = wire::Builder::new();
        remap_detach.u32(0);
        remap_detach.i32(0);
        remap_detach.i32(0);
        send(&mut client, 5, 1, remap_detach);
        send(&mut client, 5, 6, wire::Builder::new());
        let mut saw_initial = false;
        let initial_configure = loop {
            let count = client.read(&mut scratch).unwrap();
            assert!(count > 0);
            received.extend_from_slice(scratch.get(..count).unwrap());
            let mut serial = None;
            while let Some(message) = wire::take(&mut received).unwrap() {
                if message.object == 10 && message.opcode == 0 {
                    let mut args = wire::Cursor::new(&message.payload);
                    saw_initial = args.i32().unwrap() == 0 && args.i32().unwrap() == 0;
                    assert_eq!(args.u32().unwrap(), 0);
                    args.finish().unwrap();
                } else if message.object == 9 && message.opcode == 0 {
                    let mut args = wire::Cursor::new(&message.payload);
                    let candidate = args.u32().unwrap();
                    args.finish().unwrap();
                    if saw_initial {
                        serial = Some(candidate);
                    }
                    saw_initial = false;
                }
            }
            if let Some(serial) = serial {
                break serial;
            }
        };
        let mut ack = wire::Builder::new();
        ack.u32(initial_configure);
        send(&mut client, 9, 4, ack);

        let mut attach = wire::Builder::new();
        attach.u32(7);
        attach.i32(0);
        attach.i32(0);
        send(&mut client, 5, 1, attach);
        send(&mut client, 5, 6, wire::Builder::new());
        let mut saw_second_release = false;
        while !saw_second_release {
            let count = client.read(&mut scratch).unwrap();
            assert!(count > 0);
            received.extend_from_slice(scratch.get(..count).unwrap());
            while let Some(message) = wire::take(&mut received).unwrap() {
                if message.object == 7 && message.opcode == 0 {
                    saw_second_release = true;
                }
            }
        }
        let inactive_frame = fs::read(&framebuffer_path).unwrap();
        assert!(!inactive_frame
            .as_chunks::<4>()
            .0
            .contains(&[0x11, 0x22, 0x33, 0]));
        runtime
            .lock()
            .unwrap()
            .command(Command::SwitchWorkspace(2))
            .unwrap();
        let restored_frame = fs::read(&framebuffer_path).unwrap();
        assert!(restored_frame
            .as_chunks::<4>()
            .0
            .contains(&[0x11, 0x22, 0x33, 0]));

        drop(client);
        worker.join().unwrap().unwrap();
        fs::remove_file(framebuffer_path).unwrap();
        fs::remove_file(pool_path).unwrap();
    }

    /// The terminal presenting against the REAL server. Every other test of
    /// `term_client` hands `dispatch` events it built itself, so this is the
    /// only one that would catch a wrong opcode, a wrong object id, a
    /// mis-ordered request, or an event arm that is missing.
    ///
    /// It is also the whole argument for the ORDER: a client that attached no
    /// buffer would never be mapped, would receive only the initial zero
    /// configure, and would announce its own 80x24 default. Having presented,
    /// this one is mapped and holds the tile the compositor actually gave it —
    /// so the grid asserted below is the compositor's and not the fallback,
    /// and swapping the frame back behind readiness makes it the fallback
    /// again.
    #[test]
    fn td_term_presents_a_frame_and_sizes_a_pty_to_the_grid_it_was_given() {
        let stem = format!(
            "td-term-integration-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer =
            Framebuffer::test_file(&framebuffer_path, 640, 400 + BAR_HEIGHT, 640 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        runtime.lock().unwrap().repaint().unwrap();
        let (server, client) = UnixStream::pair().unwrap();
        let thread_runtime = Arc::clone(&runtime);
        let keymap = test_keymap();
        let worker = thread::spawn(move || serve_client(server, 78, thread_runtime, keymap));

        let (connection, prepared) = crate::term_client::prepare_for_test(
            client,
            &std::env::temp_dir(),
            std::path::Path::new(crate::pty::DEV_PTMX),
        )
        .unwrap();
        let (pty, size, cells) = (&prepared.pty, prepared.size().unwrap(), prepared.cells);
        let font = crate::font::pinned().unwrap();

        // The tile this output gives one surface, not the client's own guess.
        assert_eq!(
            size,
            crate::term_client::Size {
                width: 592,
                height: 332
            }
        );
        assert_eq!(
            runtime.lock().unwrap().surface_size(SurfaceKey {
                client: 78,
                object: 7,
            }),
            Some((592, 332))
        );
        assert_eq!(cells, crate::term_client::grid(size, &font).unwrap());
        assert_eq!(cells, (20, 74));
        assert_ne!(
            cells,
            crate::term_client::grid(crate::term_client::default_size(&font).unwrap(), &font)
                .unwrap(),
            "the terminal announced its fallback grid rather than the tile's"
        );
        // The whole chain in one assertion: the compositor's tile became a
        // grid, that grid was published to a real terminal, and the kernel —
        // asked again, independently of the readback `resize` already did —
        // says the terminal IS that size. §12 requires the readiness line to
        // name a grid something was actually set to; this is what makes that
        // more than a claim about arithmetic.
        let window = pty.window().unwrap();
        assert_eq!((window.rows, window.columns), cells);
        assert_eq!((window.rows, window.columns), (20, 74));
        assert_eq!(
            crate::ready::marker(window.rows, window.columns),
            "TD-TERM-READY rows=20 columns=74\n"
        );
        // A frame that was released and presented is a frame that reached the
        // framebuffer. Compared against the DESKTOP BACKGROUND, which is what
        // every pixel held before the client connected — comparing against
        // zero would pass on the bare desktop and prove nothing. These are
        // MEMORY bytes, not an RGB literal: XRGB8888 is little-endian, so the
        // blue channel comes first, as `scene`'s own desktop test spells it.
        // Counted INSIDE THE CLIENT'S OWN AREA only. Scanning the whole
        // output would let the status bar's own 640x24 band — none of it the
        // desktop colour — stand in for 15360 pixels the client never
        // painted, which is about twenty-six terminal rows of slack in an
        // assertion whose whole job is to prove the tile was covered; and
        // scanning the whole TILE would now let the title band do the same
        // for another 592x20 the compositor painted rather than the client.
        let painted = fs::read(&framebuffer_path).unwrap();
        // The client's own rectangle, derived from the constants rather than
        // from the tile being centred: the band makes it no longer symmetric
        // about the tiling area, so the old halving would have quietly moved
        // the window it scans off the client and onto the desktop below it.
        let (client_width, client_height) = (592usize, 332usize);
        let output_width = 640usize;
        let left = (output_width - client_width) / 2;
        let top = BAR_HEIGHT + GAP + TITLE_HEIGHT;
        let stride = output_width * 4;
        let mut foreign = 0usize;
        for y in top..top + client_height {
            for x in left..left + client_width {
                let offset = y * stride + x * 4;
                if painted.get(offset..offset + 4) != Some(&[0x30, 0x25, 0x20, 0][..]) {
                    foreign += 1;
                }
            }
        }
        assert_eq!(
            foreign,
            client_width * client_height,
            "the presented frame left desktop showing inside its own client area"
        );

        drop(connection);
        worker.join().unwrap().unwrap();
        fs::remove_file(&framebuffer_path).unwrap();
    }

    #[test]
    fn td_ui_demo_completes_the_real_server_handshake_and_frame() {
        let stem = format!(
            "td-ui-demo-integration-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer =
            Framebuffer::test_file(&framebuffer_path, 640, 400 + BAR_HEIGHT, 640 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        runtime.lock().unwrap().repaint().unwrap();
        let (server, client) = UnixStream::pair().unwrap();
        let thread_runtime = Arc::clone(&runtime);
        let keymap = test_keymap();
        let worker = thread::spawn(move || serve_client(server, 77, thread_runtime, keymap));

        let mut connected = crate::client::present_for_test(client, &std::env::temp_dir()).unwrap();
        let frame = fs::read(&framebuffer_path).unwrap();
        assert!(frame.as_chunks::<4>().0.contains(&[0x78, 0x46, 0xe8, 0]));
        assert_eq!(
            runtime.lock().unwrap().surface_size(SurfaceKey {
                client: 77,
                object: 7,
            }),
            Some((592, 332))
        );

        runtime
            .lock()
            .unwrap()
            .commit(
                SurfaceKey {
                    client: 88,
                    object: 1,
                },
                Surface {
                    width: 1,
                    height: 1,
                    pixels: vec![1, 2, 3, 0],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        connected.wait_for((284, 332), false, false).unwrap();
        assert_eq!(
            runtime.lock().unwrap().surface_size(SurfaceKey {
                client: 77,
                object: 7,
            }),
            Some((284, 332))
        );

        runtime
            .lock()
            .unwrap()
            .command(Command::Focus(Direction::Left))
            .unwrap();
        connected.wait_for((284, 332), true, false).unwrap();
        runtime
            .lock()
            .unwrap()
            .command(Command::ToggleFullscreen)
            .unwrap();
        connected.wait_for((640, 400), true, true).unwrap();
        runtime
            .lock()
            .unwrap()
            .command(Command::SwitchWorkspace(2))
            .unwrap();
        connected.wait_for((640, 400), false, false).unwrap();
        runtime
            .lock()
            .unwrap()
            .command(Command::SwitchWorkspace(1))
            .unwrap();
        connected.wait_for((640, 400), true, true).unwrap();
        let before_input = fs::read(&framebuffer_path).unwrap();
        runtime
            .lock()
            .unwrap()
            .pointer_frame(
                100,
                40,
                // Past the status bar, so the LOCAL coordinate the client is
                // asserted to receive is still (40, 40).
                40 + i32::try_from(BAR_HEIGHT).unwrap(),
                &[PointerButtonInput {
                    time: 100,
                    button: 0x110,
                    state: PointerButtonState::Pressed,
                }],
                PointerScroll::default(),
            )
            .unwrap();
        // Motion owes a paint rather than taking one; the reader batch flushes.
        assert_eq!(fs::read(&framebuffer_path).unwrap(), before_input);
        runtime.lock().unwrap().flush_paint().unwrap();
        let compositor_pointer_frame = fs::read(&framebuffer_path).unwrap();
        assert_ne!(compositor_pointer_frame, before_input);
        connected
            .wait_for_pointer((40 * 256, 40 * 256), 0x110)
            .unwrap();
        let pointer_frame = fs::read(&framebuffer_path).unwrap();
        assert_ne!(pointer_frame, compositor_pointer_frame);

        runtime
            .lock()
            .unwrap()
            .key(KeyInput {
                time: 101,
                key: 30,
                state: KeyState::Pressed,
            })
            .unwrap();
        connected
            .wait_for_key(30, crate::ui::UiKeyState::Pressed)
            .unwrap();
        let frame = fs::read(&framebuffer_path).unwrap();
        assert_ne!(frame, pointer_frame);

        drop(connected);
        worker.join().unwrap().unwrap();
        fs::remove_file(framebuffer_path).unwrap();
    }

    /// `set_title` was read for wire validity and dropped, so nothing the
    /// compositor held could name a window. This drives the real request
    /// through `dispatch` rather than calling `Scene::set_title`, because what
    /// broke before was the ARM and not the storage.
    #[test]
    fn set_title_reaches_the_scene_and_set_app_id_still_does_not() {
        let stem = format!(
            "td-wayland-set-title-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer =
            Framebuffer::test_file(&framebuffer_path, 120, 80 + BAR_HEIGHT, 120 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let key = SurfaceKey {
            client: 88,
            object: 5,
        };
        let (server, _peer) = UnixStream::pair().unwrap();
        let mut client = Client::new(88, server, Arc::clone(&runtime), test_keymap()).unwrap();
        client
            .insert(
                9,
                Object::XdgSurface {
                    surface: 5,
                    toplevel: Some(10),
                    configure: Arc::new(Mutex::new(ConfigureTracker::new())),
                },
            )
            .unwrap();
        client
            .insert(
                10,
                Object::XdgToplevel {
                    xdg_surface: 9,
                    decoration: None,
                },
            )
            .unwrap();

        let mut title = wire::Builder::new();
        title.string("TD-TERM").unwrap();
        client
            .dispatch(request(10, 2, title).unwrap(), &mut VecDeque::new())
            .unwrap();
        assert_eq!(runtime.lock().unwrap().title(key), Some("TD-TERM".to_string()));

        // The title follows the toplevel, so a second one replaces it.
        let mut renamed = wire::Builder::new();
        renamed.string("TD-TERM - BUILD").unwrap();
        client
            .dispatch(request(10, 2, renamed).unwrap(), &mut VecDeque::new())
            .unwrap();
        assert_eq!(
            runtime.lock().unwrap().title(key),
            Some("TD-TERM - BUILD".to_string())
        );

        // set_app_id is still read and dropped: it is not what a title bar
        // shows, and accepting it here would make the two indistinguishable.
        let mut app_id = wire::Builder::new();
        app_id.string("org.td.terminal").unwrap();
        client
            .dispatch(request(10, 3, app_id).unwrap(), &mut VecDeque::new())
            .unwrap();
        assert_eq!(
            runtime.lock().unwrap().title(key),
            Some("TD-TERM - BUILD".to_string())
        );

        // Destroying the TOPLEVEL takes the name, where unmapping its surface
        // does not. The wl_surface outlives the role object, so a toplevel
        // created on it next must not inherit this one's name.
        client
            .dispatch(
                request(10, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(runtime.lock().unwrap().title(key), None);

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// A client, its runtime, and a toplevel on a surface with no pixels — the
    /// state every decoration test starts from. The peer is returned so a test
    /// can read what the compositor sent, and the framebuffer path so it can be
    /// cleaned up.
    fn decoration_fixture(stem: &str) -> (Client, UnixStream, Arc<Mutex<Runtime>>, PathBuf) {
        let stem = format!(
            "td-wayland-{stem}-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer =
            Framebuffer::test_file(&framebuffer_path, 120, 80 + BAR_HEIGHT, 120 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut client = Client::new(88, server, Arc::clone(&runtime), test_keymap()).unwrap();
        client
            .insert(5, Object::Surface(SurfaceState::default()))
            .unwrap();
        client
            .insert(
                9,
                Object::XdgSurface {
                    surface: 5,
                    toplevel: Some(10),
                    configure: Arc::new(Mutex::new(ConfigureTracker::new())),
                },
            )
            .unwrap();
        client
            .insert(
                10,
                Object::XdgToplevel {
                    xdg_surface: 9,
                    decoration: None,
                },
            )
            .unwrap();
        client.insert(11, Object::DecorationManager).unwrap();
        (client, peer, runtime, framebuffer_path)
    }

    /// `get_toplevel_decoration(id, toplevel)` on the manager at object 11.
    fn get_decoration(client: &mut Client, id: u32, toplevel: u32) -> Result<(), String> {
        let mut request_body = wire::Builder::new();
        request_body.u32(id);
        request_body.u32(toplevel);
        client.dispatch(request(11, 1, request_body).unwrap(), &mut VecDeque::new())
    }

    /// The mode carried by the next `configure`, which is the only event this
    /// interface has — so a message arriving on the decoration object at all is
    /// one of these.
    fn configured_mode(peer: &mut UnixStream, decoration: u32) -> u32 {
        let messages = receive_messages(peer, 1);
        let message = messages.first().unwrap();
        assert_eq!(message.object, decoration);
        assert_eq!(message.opcode, DECORATION_CONFIGURE);
        let mut payload = wire::Cursor::new(&message.payload);
        let mode = payload.u32().unwrap();
        payload.finish().unwrap();
        mode
    }

    /// §F of APPLICATIONS.md states td's advertised globals as a fact about
    /// this function, and a table in a document cannot notice when the code
    /// moves under it. This is that claim as a test: the SET, the order, the
    /// name each is advertised under and the version each is offered at.
    #[test]
    fn the_registry_advertises_exactly_the_globals_td_serves() {
        let stem = format!(
            "td-wayland-globals-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer =
            Framebuffer::test_file(&framebuffer_path, 120, 80 + BAR_HEIGHT, 120 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut client = Client::new(88, server, runtime, test_keymap()).unwrap();
        client.advertise_globals(2).unwrap();

        let globals = receive_messages(&mut peer, GLOBAL_COUNT);
        let advertised: Vec<(u32, String, u32)> = globals
            .iter()
            .map(|message| {
                let mut payload = wire::Cursor::new(&message.payload);
                let name = payload.u32().unwrap();
                let interface = payload.string().unwrap();
                let version = payload.u32().unwrap();
                payload.finish().unwrap();
                (name, interface, version)
            })
            .collect();
        assert_eq!(
            advertised,
            vec![
                (GLOBAL_COMPOSITOR, "wl_compositor".to_string(), 4),
                (GLOBAL_SHM, "wl_shm".to_string(), 1),
                (GLOBAL_OUTPUT, "wl_output".to_string(), 4),
                (GLOBAL_XDG_WM_BASE, "xdg_wm_base".to_string(), 1),
                (
                    GLOBAL_DECORATION,
                    "zxdg_decoration_manager_v1".to_string(),
                    1
                ),
                (GLOBAL_SEAT, "wl_seat".to_string(), SEAT_VERSION),
            ]
        );

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// The numbers this interface is spoken in, pinned by VALUE. Every other
    /// test here compares what went out against these same constants, so a
    /// constant that agrees with the code and not with the protocol is one they
    /// would all still pass — and the client on the other end, which knows the
    /// real numbers, would read `server_side` as `client_side` and draw its own
    /// titlebar anyway.
    #[test]
    fn the_decoration_protocol_numbers_are_the_protocols() {
        assert_eq!(DECORATION_MODE_CLIENT_SIDE, 1);
        assert_eq!(DECORATION_MODE_SERVER_SIDE, 2);
        assert_eq!(DECORATION_ERROR_UNCONFIGURED_BUFFER, 0);
        assert_eq!(DECORATION_ERROR_ALREADY_CONSTRUCTED, 1);
        assert_eq!(DECORATION_ERROR_ORPHANED, 2);
        assert_eq!(DECORATION_CONFIGURE, 0);
    }

    /// The whole point of the interface for a tiling compositor: the client is
    /// told, before it has drawn anything, that it is not the one drawing the
    /// titlebar.
    #[test]
    fn a_toplevel_is_told_the_compositor_draws_its_decorations() {
        let (mut client, mut peer, _runtime, framebuffer_path) =
            decoration_fixture("decoration-configure");

        get_decoration(&mut client, 12, 10).unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// `set_mode` is a PREFERENCE, and the compositor's answer need not agree.
    /// A client asking to draw its own titlebar is told `server_side` anyway —
    /// which is the case that would double the title band if td deferred to it.
    #[test]
    fn a_client_asking_to_draw_its_own_titlebar_is_still_told_server_side() {
        let (mut client, mut peer, _runtime, framebuffer_path) =
            decoration_fixture("decoration-set-mode");

        get_decoration(&mut client, 12, 10).unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        let mut set_mode = wire::Builder::new();
        set_mode.u32(DECORATION_MODE_CLIENT_SIDE);
        client
            .dispatch(request(12, 1, set_mode).unwrap(), &mut VecDeque::new())
            .unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        // unset_mode is the client withdrawing the preference, and is answered
        // the same way: the answer never depended on it.
        client
            .dispatch(
                request(12, 2, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// Answering EVERY ask is what this pins, rather than answering the first:
    /// `configure` is the only way the mode is carried, so a compositor that
    /// stayed silent on a repeat request would leave a client that asked twice
    /// waiting on an event that never comes.
    #[test]
    fn a_mode_td_already_serves_is_still_answered() {
        let (mut client, mut peer, _runtime, framebuffer_path) =
            decoration_fixture("decoration-same-mode");

        get_decoration(&mut client, 12, 10).unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        let mut set_mode = wire::Builder::new();
        set_mode.u32(DECORATION_MODE_SERVER_SIDE);
        client
            .dispatch(request(12, 1, set_mode).unwrap(), &mut VecDeque::new())
            .unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// A mode outside the enum is the client's mistake and is refused rather
    /// than rounded to the answer td was going to give anyway — which would
    /// tell a client that sent 0 for `server_side` that it had been understood.
    #[test]
    fn a_decoration_mode_outside_the_enum_is_refused() {
        let (mut client, mut peer, _runtime, framebuffer_path) =
            decoration_fixture("decoration-bad-mode");

        get_decoration(&mut client, 12, 10).unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        let mut set_mode = wire::Builder::new();
        set_mode.u32(0);
        let error = client
            .dispatch(request(12, 1, set_mode).unwrap(), &mut VecDeque::new())
            .unwrap_err();
        assert!(error.contains("undefined mode 0"), "{error}");
        // Refused INSTEAD of answered: a configure beside the error would tell
        // the client the request it is being disconnected over was understood.
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut scratch = [0u8; 64];
        assert!(
            peer.read(&mut scratch).is_err(),
            "a refused mode owes no configure"
        );

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// `already_constructed`, by its own code rather than as a disconnect with
    /// nothing on it: the client asked for a second decoration on one toplevel.
    #[test]
    fn a_second_decoration_for_one_toplevel_is_refused() {
        let (mut client, mut peer, _runtime, framebuffer_path) =
            decoration_fixture("decoration-twice");

        get_decoration(&mut client, 12, 10).unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        let error = get_decoration(&mut client, 13, 10).unwrap_err();
        assert!(error.contains("already has decoration 12"), "{error}");
        assert_eq!(
            client.protocol_error_code,
            DECORATION_ERROR_ALREADY_CONSTRUCTED
        );
        // Refused BEFORE the object was made, so the id the client named is
        // still free — it is holding nothing the compositor also thinks exists.
        assert!(!client.objects.contains_key(&13));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// Destroying the decoration gives the slot back, so the refusal above is
    /// about one being LIVE rather than about one ever having existed.
    #[test]
    fn destroying_a_decoration_lets_the_toplevel_take_another() {
        let (mut client, mut peer, _runtime, framebuffer_path) =
            decoration_fixture("decoration-reask");

        get_decoration(&mut client, 12, 10).unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);
        client
            .dispatch(
                request(12, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        // The destroy is acknowledged with wl_display.delete_id before the next
        // decoration's configure, so the read below must skip it.
        receive_messages(&mut peer, 1);

        get_decoration(&mut client, 13, 10).unwrap();
        assert_eq!(configured_mode(&mut peer, 13), DECORATION_MODE_SERVER_SIDE);

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// `unconfigured_buffer`. The protocol refuses a decoration for a window
    /// that already has pixels, and the ordinary way to reach it is a client
    /// that mapped itself and asked afterwards — so the buffer this checks for
    /// is a COMMITTED one, which has left the server's pending state and is the
    /// scene's.
    #[test]
    fn decorating_a_window_that_already_has_pixels_is_refused() {
        let (mut client, _peer, runtime, framebuffer_path) =
            decoration_fixture("decoration-mapped");
        let key = SurfaceKey {
            client: 88,
            object: 5,
        };
        runtime
            .lock()
            .unwrap()
            .commit(
                key,
                Surface {
                    width: 1,
                    height: 1,
                    pixels: vec![1, 2, 3, 0],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        assert!(runtime.lock().unwrap().is_mapped(key));

        let error = get_decoration(&mut client, 12, 10).unwrap_err();
        assert!(error.contains("had a buffer"), "{error}");
        assert_eq!(
            client.protocol_error_code,
            DECORATION_ERROR_UNCONFIGURED_BUFFER
        );
        assert!(!client.objects.contains_key(&12));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// The same refusal for the other half of "has a buffer": one ATTACHED and
    /// not yet committed, which never reaches the scene and so is invisible to
    /// the query above.
    #[test]
    fn decorating_a_window_with_a_buffer_attached_is_refused_too() {
        let (mut client, _peer, _runtime, framebuffer_path) =
            decoration_fixture("decoration-attached");
        let pool_path = framebuffer_path.with_extension("pool");
        fs::write(&pool_path, [1u8, 2, 3, 0]).unwrap();
        client.objects.insert(
            5,
            Object::Surface(SurfaceState {
                pending_buffer: Some(PendingBuffer::Buffer {
                    object: 7,
                    buffer: Buffer {
                        serial: 1,
                        file: Arc::new(File::open(&pool_path).unwrap()),
                        offset: 0,
                        width: 1,
                        height: 1,
                        stride: 4,
                        format: SHM_XRGB8888,
                    },
                }),
                ..SurfaceState::default()
            }),
        );

        let error = get_decoration(&mut client, 12, 10).unwrap_err();
        assert!(error.contains("had a buffer"), "{error}");
        assert_eq!(
            client.protocol_error_code,
            DECORATION_ERROR_UNCONFIGURED_BUFFER
        );
        assert!(!client.objects.contains_key(&12));

        let _ = fs::remove_file(&pool_path);
        let _ = fs::remove_file(&framebuffer_path);
    }

    /// `orphaned`: the decoration must go before the thing it decorates. The
    /// toplevel is left INTACT by the refusal — it is the object the diagnostic
    /// names, and tearing it down while telling the client it should not have
    /// would leave the two disagreeing about what still exists.
    #[test]
    fn a_toplevel_destroyed_under_its_decoration_is_orphaned() {
        let (mut client, mut peer, _runtime, framebuffer_path) =
            decoration_fixture("decoration-orphan");

        get_decoration(&mut client, 12, 10).unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        let error = client
            .dispatch(
                request(10, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap_err();
        assert!(error.contains("before its decoration 12"), "{error}");
        assert_eq!(client.protocol_error_code, DECORATION_ERROR_ORPHANED);
        assert!(client.objects.contains_key(&10));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// Destroying them in the order the protocol asks for works, which is what
    /// makes the refusal above about ORDER rather than about decorations
    /// blocking a toplevel from ever being destroyed.
    #[test]
    fn a_decorated_toplevel_is_destroyed_once_its_decoration_is() {
        let (mut client, mut peer, _runtime, framebuffer_path) =
            decoration_fixture("decoration-order");

        get_decoration(&mut client, 12, 10).unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);
        client
            .dispatch(
                request(12, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        client
            .dispatch(
                request(10, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(!client.objects.contains_key(&10));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// Wayland scopes an error CODE to the INTERFACE of the object it names, so
    /// which object goes out is half the meaning. `already_constructed` is
    /// raised from the MANAGER's request, and the manager defines no errors at
    /// all — a client told code 1 against it can only read an undefined error.
    /// It names the decoration the client just asked for instead, whose proxy
    /// the client already holds.
    #[test]
    fn a_creation_error_names_the_decoration_rather_than_the_manager() {
        let (mut client, mut peer, _runtime, framebuffer_path) =
            decoration_fixture("decoration-error-object");

        get_decoration(&mut client, 12, 10).unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        let mut again = wire::Builder::new();
        again.u32(13);
        again.u32(10);
        let mut bytes = again.message(11, 1).unwrap();
        let error = dispatch_buffered(&mut client, &mut bytes, &mut VecDeque::new()).unwrap_err();
        assert!(error.contains("already has decoration 12"), "{error}");

        let events = receive_messages(&mut peer, 1);
        let event = events.first().unwrap();
        assert_eq!((event.object, event.opcode), (1, 0), "wl_display.error");
        let mut payload = wire::Cursor::new(&event.payload);
        assert_eq!(
            payload.u32().unwrap(),
            13,
            "the error names the decoration, not the manager that raised it"
        );
        assert_eq!(payload.u32().unwrap(), DECORATION_ERROR_ALREADY_CONSTRUCTED);

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// The same rule, on the case where getting it wrong is not merely
    /// undefined but WRONG: `orphaned` is 2, and 2 on an `xdg_toplevel` is
    /// `invalid_size`. Named against the toplevel whose destroy raised it, a
    /// client would be told its window was the wrong size.
    #[test]
    fn the_orphaned_error_names_the_decoration_and_not_the_toplevel() {
        let (mut client, mut peer, _runtime, framebuffer_path) =
            decoration_fixture("decoration-orphan-object");

        get_decoration(&mut client, 12, 10).unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        let mut bytes = wire::Builder::new().message(10, 0).unwrap();
        let error = dispatch_buffered(&mut client, &mut bytes, &mut VecDeque::new()).unwrap_err();
        assert!(error.contains("before its decoration 12"), "{error}");

        let events = receive_messages(&mut peer, 1);
        let event = events.first().unwrap();
        assert_eq!((event.object, event.opcode), (1, 0), "wl_display.error");
        let mut payload = wire::Cursor::new(&event.payload);
        assert_eq!(
            payload.u32().unwrap(),
            12,
            "orphaned belongs to the decoration; on the toplevel 2 is invalid_size"
        );
        assert_eq!(payload.u32().unwrap(), DECORATION_ERROR_ORPHANED);

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// A mode is applied on the `xdg_surface.configure` that FOLLOWS it, and
    /// the client acknowledges that serial — so answering `set_mode` with the
    /// decoration event alone leaves a mapped window waiting and still drawing
    /// its own titlebar. The layout has not moved, so the tracker would
    /// deduplicate the configure away; this is the assertion that it is asked
    /// for anyway.
    #[test]
    fn setting_a_mode_owes_the_surface_configure_the_client_acknowledges() {
        let (mut client, mut peer, _runtime, framebuffer_path) =
            decoration_fixture("decoration-reconfigure");
        let Some(Object::XdgSurface { configure, .. }) = client.objects.get(&9).cloned() else {
            panic!("fixture lost its xdg_surface");
        };

        get_decoration(&mut client, 12, 10).unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        let mapped = ViewStatus::Visible(ToplevelState {
            width: 100,
            height: 100,
            activated: true,
            fullscreen: false,
        });
        configure.lock().unwrap().initial(1).unwrap();
        configure.lock().unwrap().acknowledge(1).unwrap();
        assert!(configure
            .lock()
            .unwrap()
            .update(mapped, 2)
            .unwrap()
            .is_some());
        // Still: the next update is deduplicated, which is what would swallow
        // the configure the mode answer owes.
        assert!(configure
            .lock()
            .unwrap()
            .update(mapped, 3)
            .unwrap()
            .is_none());

        let mut set_mode = wire::Builder::new();
        set_mode.u32(DECORATION_MODE_CLIENT_SIDE);
        client
            .dispatch(request(12, 1, set_mode).unwrap(), &mut VecDeque::new())
            .unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        let owed = configure
            .lock()
            .unwrap()
            .update(mapped, 4)
            .unwrap()
            .expect("set_mode owes an xdg_surface.configure for the client to ack");
        assert_eq!(owed.serial, 4);
        // The layout itself is untouched: a window is not resized because its
        // titlebar was discussed.
        assert_eq!((owed.state.width, owed.state.height), (100, 100));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// The legal-but-unusual ordering that makes creation need the same answer
    /// `set_mode` does: commit empty, ACKNOWLEDGE the initial configure, and
    /// only then ask for a decoration. `unconfigured_buffer` permits it — no
    /// buffer has been attached — so the mapping configure this would otherwise
    /// ride on has already been spent, and the client would draw its first
    /// frame against a mode it never got an ack-able serial for.
    #[test]
    fn creating_a_decoration_after_the_initial_ack_still_owes_a_configure() {
        let (mut client, mut peer, _runtime, framebuffer_path) =
            decoration_fixture("decoration-late-create");
        let Some(Object::XdgSurface { configure, .. }) = client.objects.get(&9).cloned() else {
            panic!("fixture lost its xdg_surface");
        };
        let mapped = ViewStatus::Visible(ToplevelState {
            width: 100,
            height: 100,
            activated: true,
            fullscreen: false,
        });
        configure.lock().unwrap().initial(1).unwrap();
        configure.lock().unwrap().acknowledge(1).unwrap();
        assert!(configure
            .lock()
            .unwrap()
            .update(mapped, 2)
            .unwrap()
            .is_some());
        assert!(configure
            .lock()
            .unwrap()
            .update(mapped, 3)
            .unwrap()
            .is_none());

        get_decoration(&mut client, 12, 10).unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        assert!(
            configure
                .lock()
                .unwrap()
                .update(mapped, 4)
                .unwrap()
                .is_some(),
            "creating a decoration owes the configure its mode is applied on"
        );

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// The protocol says so explicitly, and it is the manager's ONE lifetime
    /// rule: destroying it is not destroying the decorations it made. A client
    /// that binds the manager, decorates its window and drops the manager it no
    /// longer needs must keep the decoration it asked for.
    #[test]
    fn destroying_the_manager_leaves_the_decorations_it_made() {
        let (mut client, mut peer, _runtime, framebuffer_path) =
            decoration_fixture("decoration-manager-destroy");

        get_decoration(&mut client, 12, 10).unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        client
            .dispatch(
                request(11, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        receive_messages(&mut peer, 1);
        assert!(!client.objects.contains_key(&11));

        // Still a decoration, and still answering.
        client
            .dispatch(
                request(12, 2, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(configured_mode(&mut peer, 12), DECORATION_MODE_SERVER_SIDE);

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// The manager refuses an id that is not a toplevel rather than making a
    /// decoration that decorates nothing.
    #[test]
    fn a_decoration_for_something_that_is_not_a_toplevel_is_refused() {
        let (mut client, _peer, _runtime, framebuffer_path) =
            decoration_fixture("decoration-non-toplevel");

        let error = get_decoration(&mut client, 12, 5).unwrap_err();
        assert!(error.contains("non-toplevel 5"), "{error}");
        assert!(!client.objects.contains_key(&12));

        let _ = fs::remove_file(&framebuffer_path);
    }

    #[test]
    fn ack_dispatch_wakes_the_latest_configure_after_backpressure() {
        let stem = format!(
            "td-wayland-configure-backpressure-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer =
            Framebuffer::test_file(&framebuffer_path, 120, 80 + BAR_HEIGHT, 120 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let key = SurfaceKey {
            client: 88,
            object: 5,
        };
        runtime
            .lock()
            .unwrap()
            .commit(
                key,
                Surface {
                    width: 1,
                    height: 1,
                    pixels: vec![1, 2, 3, 0],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();

        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut client = Client::new(88, server, Arc::clone(&runtime), test_keymap()).unwrap();
        let tracker = Arc::new(Mutex::new(ConfigureTracker::new()));
        {
            let mut tracker = tracker.lock().unwrap();
            tracker.initial(999).unwrap();
            tracker.acknowledge(999).unwrap();
        }
        client
            .insert(
                9,
                Object::XdgSurface {
                    surface: 5,
                    toplevel: Some(10),
                    configure: Arc::clone(&tracker),
                },
            )
            .unwrap();
        client
            .insert(
                10,
                Object::XdgToplevel {
                    xdg_surface: 9,
                    decoration: None,
                },
            )
            .unwrap();
        client.configurations.lock().unwrap().insert(
            key,
            ConfigureRegistration {
                xdg_surface: 9,
                toplevel: 10,
                tracker: Arc::clone(&tracker),
            },
        );

        let subscription = runtime.lock().unwrap().subscribe(88).unwrap();
        let (receiver, stop) = subscription.split();
        let worker_stop = stop.clone();
        let worker_runtime = Arc::clone(&runtime);
        let configurations = Arc::clone(&client.configurations);
        let outbound = Arc::clone(&client.outbound);
        let worker = thread::spawn(move || {
            configure_worker(
                receiver,
                worker_stop,
                worker_runtime,
                configurations,
                outbound,
            )
        });

        let mut received = Vec::new();
        let (primed_size, primed_serial, primed_activated) =
            receive_configure(&mut peer, &mut received, 9, 10);
        assert_eq!(primed_size, (72, 12));
        assert!(primed_activated);
        {
            let mut tracker = tracker.lock().unwrap();
            tracker.acknowledge(primed_serial).unwrap();
            for offset in 0..crate::configure::MAX_OUTSTANDING {
                let width = i32::try_from(offset).unwrap().saturating_add(1);
                let serial = u32::try_from(offset).unwrap().saturating_add(1000);
                assert!(tracker
                    .update(
                        ViewStatus::Visible(ToplevelState {
                            width,
                            height: 10,
                            activated: false,
                            fullscreen: false,
                        }),
                        serial,
                    )
                    .unwrap()
                    .is_some());
            }
        }

        let mut ack = wire::Builder::new();
        ack.u32(1000);
        client
            .dispatch(request(9, 4, ack).unwrap(), &mut VecDeque::new())
            .unwrap();
        let (size, serial, activated) = receive_configure(&mut peer, &mut received, 9, 10);
        assert_eq!(size, (72, 12));
        assert!(activated);
        assert_ne!(serial, 0);

        stop.stop();
        worker.join().unwrap().unwrap();
        runtime.lock().unwrap().unsubscribe(88);
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn event_write_after_peer_departure_is_clean_disconnect() {
        let framebuffer_path = std::env::temp_dir().join(format!(
            "td-wayland-disconnect-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 4, 4, 16).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, peer) = UnixStream::pair().unwrap();
        let mut client = Client::new(1, server, runtime, test_keymap()).unwrap();
        drop(peer);

        let mut first = wire::Builder::new();
        first.u32(2);
        client.send(1, 1, first).unwrap();
        assert!(client.disconnected().unwrap());
        let mut second = wire::Builder::new();
        second.u32(3);
        client.send(1, 1, second).unwrap();
        assert!(client.send(0, 1, wire::Builder::new()).is_err());

        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn disconnected_client_suppresses_later_writes() {
        let framebuffer_path = std::env::temp_dir().join(format!(
            "td-wayland-suppress-write-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 4, 4, 16).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut client = Client::new(1, server, runtime, test_keymap()).unwrap();
        client.outbound.lock().unwrap().disconnected = true;
        let mut event = wire::Builder::new();
        event.u32(2);

        client.send(1, 1, event).unwrap();

        let mut received = [0u8; 16];
        let error = peer.read(&mut received).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn unread_event_makes_peer_reset_a_clean_server_exit() {
        let framebuffer_path = std::env::temp_dir().join(format!(
            "td-wayland-recv-reset-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 4, 4, 16).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (mut server, peer) = UnixStream::pair().unwrap();
        server.write_all(b"unread event").unwrap();
        drop(peer);

        serve_client(server, 1, runtime, test_keymap()).unwrap();

        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn queued_request_stops_dispatch_when_its_reply_finds_closed_peer() {
        let framebuffer_path = std::env::temp_dir().join(format!(
            "td-wayland-dispatch-disconnect-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 4, 4, 16).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, peer) = UnixStream::pair().unwrap();
        let mut client = Client::new(1, server, runtime, test_keymap()).unwrap();
        let mut get_registry = wire::Builder::new();
        get_registry.u32(2);
        let mut bytes = get_registry.message(1, 1).unwrap();
        let mut fds = VecDeque::new();
        drop(peer);

        assert_eq!(
            dispatch_buffered(&mut client, &mut bytes, &mut fds).unwrap(),
            DispatchOutcome::Disconnected
        );
        assert!(bytes.is_empty());
        assert!(client.disconnected().unwrap());

        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn queued_request_with_closed_peer_makes_clean_server_exit() {
        let framebuffer_path = std::env::temp_dir().join(format!(
            "td-wayland-server-disconnect-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 4, 4, 16).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, mut peer) = UnixStream::pair().unwrap();
        let mut get_registry = wire::Builder::new();
        get_registry.u32(2);
        peer.write_all(&get_registry.message(1, 1).unwrap())
            .unwrap();
        drop(peer);

        serve_client(server, 1, runtime, test_keymap()).unwrap();

        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn pool_and_buffer_bounds_fail_closed() {
        let path = std::env::temp_dir().join(format!(
            "td-wayland-bounds-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, vec![0u8; 64]).unwrap();
        let pool = Pool {
            file: Arc::new(File::open(&path).unwrap()),
            size: 64,
        };
        let framebuffer_path = path.with_extension("fb");
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 4, 4, 16).unwrap();
        let (server, _peer) = UnixStream::pair().unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let mut client = Client::new(1, server, runtime, test_keymap()).unwrap();
        assert!(client
            .create_buffer(pool.clone(), 2, 0, 4, 4, 15, SHM_XRGB8888)
            .is_err());
        assert!(client
            .create_buffer(pool, 2, 0, 4, 5, 16, SHM_XRGB8888)
            .is_err());
        fs::remove_file(path).unwrap();
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn attached_buffer_survives_client_side_object_destruction() {
        let stem = format!(
            "td-wayland-buffer-life-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let pool_path = std::env::temp_dir().join(format!("{stem}.pool"));
        let pixels = [0x21u8, 0x43, 0x65, 0];
        fs::write(&pool_path, pixels).unwrap();
        let framebuffer =
            Framebuffer::test_file(&framebuffer_path, 8, crate::scene::least_output_height(2), 32)
                .unwrap();
        let (server, _peer) = UnixStream::pair().unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let mut client = Client::new(2, server, runtime, test_keymap()).unwrap();
        let mut configure = ConfigureTracker::new();
        configure.initial(44).unwrap();
        configure.acknowledge(44).unwrap();
        client
            .insert(
                5,
                Object::Surface(SurfaceState {
                    role: Some(SurfaceRole::Xdg(9)),
                    ..SurfaceState::default()
                }),
            )
            .unwrap();
        client
            .insert(
                9,
                Object::XdgSurface {
                    surface: 5,
                    toplevel: Some(10),
                    configure: Arc::new(Mutex::new(configure)),
                },
            )
            .unwrap();
        client
            .insert(
                10,
                Object::XdgToplevel {
                    xdg_surface: 9,
                    decoration: None,
                },
            )
            .unwrap();
        client
            .insert(
                7,
                Object::Buffer(Buffer {
                    serial: 99,
                    file: Arc::new(File::open(&pool_path).unwrap()),
                    offset: 0,
                    width: 1,
                    height: 1,
                    stride: 4,
                    format: SHM_XRGB8888,
                }),
            )
            .unwrap();

        let mut attach = wire::Builder::new();
        attach.u32(7);
        attach.i32(0);
        attach.i32(0);
        client
            .dispatch(request(5, 1, attach).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(7, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        client
            .dispatch(
                request(5, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();

        let frame = fs::read(&framebuffer_path).unwrap();
        assert!(frame.as_chunks::<4>().0.contains(&pixels));

        client
            .dispatch(
                request(10, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        let frame = fs::read(&framebuffer_path).unwrap();
        assert!(!frame.as_chunks::<4>().0.contains(&pixels));
        let reset = match client.objects.get(&9) {
            Some(Object::XdgSurface {
                toplevel: None,
                configure,
                ..
            }) => Some(Arc::clone(configure)),
            _ => None,
        }
        .unwrap();
        assert!(!reset.lock().unwrap().initial_sent());
        assert!(!reset.lock().unwrap().can_attach());

        let mut new_toplevel = wire::Builder::new();
        new_toplevel.u32(11);
        client
            .dispatch(request(9, 1, new_toplevel).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut geometry = wire::Builder::new();
        for value in [0, 0, 1, 1] {
            geometry.i32(value);
        }
        client
            .dispatch(request(9, 3, geometry).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(5, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        let configured = match client.objects.get(&9) {
            Some(Object::XdgSurface {
                toplevel: Some(11),
                configure,
                ..
            }) => Some(Arc::clone(configure)),
            _ => None,
        }
        .unwrap();
        assert!(configured.lock().unwrap().initial_sent());
        assert!(!configured.lock().unwrap().can_attach());
        fs::remove_file(framebuffer_path).unwrap();
        fs::remove_file(pool_path).unwrap();
    }

    #[test]
    fn input_region_state_is_copied_on_set_and_applied_on_commit() {
        let stem = format!(
            "td-wayland-input-region-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(
            &framebuffer_path,
            80,
            crate::scene::least_output_height(8),
            80 * 4,
        )
        .unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, _peer) = UnixStream::pair().unwrap();
        let mut client = Client::new(2, server, Arc::clone(&runtime), test_keymap()).unwrap();
        client
            .insert(5, Object::Surface(SurfaceState::default()))
            .unwrap();
        client
            .insert(6, Object::Region(Arc::new(InputRegion::new())))
            .unwrap();

        let mut add = wire::Builder::new();
        for value in [1, 2, 3, 4] {
            add.i32(value);
        }
        client
            .dispatch(request(6, 1, add).unwrap(), &mut VecDeque::new())
            .unwrap();
        let expected = match client.objects.get(&6) {
            Some(Object::Region(region)) => region.clone(),
            _ => Arc::new(InputRegion::new()),
        };
        let key = SurfaceKey {
            client: 2,
            object: 5,
        };
        runtime
            .lock()
            .unwrap()
            .commit(
                key,
                Surface {
                    width: 32,
                    height: 32,
                    pixels: vec![1; 32 * 32 * 4],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        let rect = runtime
            .lock()
            .unwrap()
            .layout_snapshot()
            .get(&key)
            .unwrap()
            .rect;
        runtime
            .lock()
            .unwrap()
            .pointer_frame(
                1,
                i32::try_from(rect.x).unwrap(),
                i32::try_from(rect.y).unwrap(),
                &[],
                PointerScroll::default(),
            )
            .unwrap();
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .pointer_snapshot()
                .focus
                .map(|target| target.surface),
            Some(key)
        );
        let mut set = wire::Builder::new();
        set.u32(6);
        client
            .dispatch(request(5, 5, set).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut later_add = wire::Builder::new();
        for value in [0, 0, 1, 1] {
            later_add.i32(value);
        }
        client
            .dispatch(request(6, 1, later_add).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(6, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(matches!(
            client.objects.get(&5),
            Some(Object::Surface(SurfaceState {
                pending_input_region: Some(Some(region)),
                ..
            })) if region == &expected
        ));

        client
            .dispatch(
                request(5, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(runtime.lock().unwrap().pointer_snapshot().focus, None);
        assert!(matches!(
            client.objects.get(&5),
            Some(Object::Surface(SurfaceState {
                pending_input_region: None,
                input_region: Some(region),
                ..
            })) if region == &expected
        ));

        let mut reset = wire::Builder::new();
        reset.u32(0);
        client
            .dispatch(request(5, 5, reset).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(5, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(matches!(
            client.objects.get(&5),
            Some(Object::Surface(SurfaceState {
                pending_input_region: None,
                input_region: None,
                ..
            }))
        ));
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn input_region_limits_are_bounded_noops_without_disconnect() {
        let stem = format!(
            "td-wayland-input-region-limit-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 8, 8, 8 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let (server, _peer) = UnixStream::pair().unwrap();
        let mut client = Client::new(3, server, runtime, test_keymap()).unwrap();

        for object in 10..26 {
            client
                .insert(object, Object::Region(Arc::new(InputRegion::new())))
                .unwrap();
            for _ in 0..MAX_INPUT_REGION_OPERATIONS {
                let mut add = wire::Builder::new();
                for value in [0, 0, 1, 1] {
                    add.i32(value);
                }
                client
                    .dispatch(request(object, 1, add).unwrap(), &mut VecDeque::new())
                    .unwrap();
            }
        }
        assert_eq!(
            client.retained_input_region_operations(),
            MAX_CLIENT_INPUT_REGION_OPERATIONS
        );

        client
            .insert(26, Object::Region(Arc::new(InputRegion::new())))
            .unwrap();
        for values in [[0, 0, 0, 1], [0, 0, 1, 1]] {
            let mut add = wire::Builder::new();
            for value in values {
                add.i32(value);
            }
            client
                .dispatch(request(26, 1, add).unwrap(), &mut VecDeque::new())
                .unwrap();
        }
        assert!(matches!(
            client.objects.get(&26),
            Some(Object::Region(region)) if region.len() == 0
        ));

        let mut excess = wire::Builder::new();
        for value in [0, 0, 1, 1] {
            excess.i32(value);
        }
        client
            .dispatch(request(10, 1, excess).unwrap(), &mut VecDeque::new())
            .unwrap();
        assert!(matches!(
            client.objects.get(&10),
            Some(Object::Region(region)) if region.len() == MAX_INPUT_REGION_OPERATIONS
        ));
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn cursor_role_accepts_commits_without_mapping_a_toplevel() {
        let stem = format!(
            "td-wayland-cursor-role-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let pool_path = std::env::temp_dir().join(format!("{stem}.pool"));
        fs::write(&pool_path, [1, 2, 3, 0]).unwrap();
        let framebuffer = Framebuffer::test_file(
            &framebuffer_path,
            80,
            crate::scene::least_output_height(8),
            80 * 4,
        )
        .unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let focused = SurfaceKey {
            client: 2,
            object: 9,
        };
        runtime
            .lock()
            .unwrap()
            .commit(
                focused,
                Surface {
                    width: 32,
                    height: 32,
                    pixels: vec![1; 32 * 32 * 4],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        let rect = runtime
            .lock()
            .unwrap()
            .layout_snapshot()
            .get(&focused)
            .unwrap()
            .rect;
        runtime
            .lock()
            .unwrap()
            .pointer_frame(
                1,
                i32::try_from(rect.x.saturating_add(2)).unwrap(),
                i32::try_from(rect.y.saturating_add(3)).unwrap(),
                &[],
                PointerScroll::default(),
            )
            .unwrap();
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut client = Client::new(2, server, Arc::clone(&runtime), test_keymap()).unwrap();
        let subscription = runtime
            .lock()
            .unwrap()
            .subscribe_input_with_activity(
                2,
                Arc::clone(&client.keyboard_active),
                Arc::clone(&client.pointer_active),
            )
            .unwrap();
        let (_receiver, stop) = subscription.split();
        client
            .insert(5, Object::Surface(SurfaceState::default()))
            .unwrap();
        client.insert(6, Object::Pointer { version: 7 }).unwrap();

        let mut set_cursor = wire::Builder::new();
        set_cursor.u32(44);
        set_cursor.u32(5);
        set_cursor.i32(1);
        set_cursor.i32(2);
        client
            .dispatch(request(6, 0, set_cursor).unwrap(), &mut VecDeque::new())
            .unwrap();
        assert!(matches!(
            client.objects.get(&5),
            Some(Object::Surface(SurfaceState { role: None, .. }))
        ));
        client.objects.remove(&6);
        client.create_pointer(6, 7).unwrap();
        let first_enter = receive_messages(&mut peer, 2);
        let mut first = wire::Cursor::new(&first_enter.first().unwrap().payload);
        let serial = first.u32().unwrap();
        assert_eq!(first.u32().unwrap(), focused.object);
        client.create_pointer(10, 7).unwrap();
        let second_enter = receive_messages(&mut peer, 2);
        let mut second = wire::Cursor::new(&second_enter.first().unwrap().payload);
        assert_eq!(second.u32().unwrap(), serial);
        assert_eq!(second.u32().unwrap(), focused.object);
        assert_eq!(
            *client.pointer_authority.lock().unwrap(),
            Some(PointerEnterAuthority {
                serial,
                surface: focused.object,
                after_revision: 1,
            })
        );

        runtime
            .lock()
            .unwrap()
            .pointer_frame(2, 1_000, 1_000, &[], PointerScroll::default())
            .unwrap();
        let mut after_leave = wire::Builder::new();
        after_leave.u32(serial);
        after_leave.u32(5);
        after_leave.i32(1);
        after_leave.i32(2);
        client
            .dispatch(request(6, 0, after_leave).unwrap(), &mut VecDeque::new())
            .unwrap();
        assert!(matches!(
            client.objects.get(&5),
            Some(Object::Surface(SurfaceState { role: None, .. }))
        ));
        runtime
            .lock()
            .unwrap()
            .pointer_frame(3, -1_000, -1_000, &[], PointerScroll::default())
            .unwrap();
        runtime
            .lock()
            .unwrap()
            .pointer_frame(
                4,
                i32::try_from(rect.x.saturating_add(2)).unwrap(),
                i32::try_from(rect.y.saturating_add(3)).unwrap(),
                &[],
                PointerScroll::default(),
            )
            .unwrap();
        let mut set_cursor = wire::Builder::new();
        set_cursor.u32(serial);
        set_cursor.u32(5);
        set_cursor.i32(1);
        set_cursor.i32(2);
        client
            .dispatch(request(6, 0, set_cursor).unwrap(), &mut VecDeque::new())
            .unwrap();
        assert!(matches!(
            client.objects.get(&5),
            Some(Object::Surface(SurfaceState {
                role: Some(SurfaceRole::Cursor),
                ..
            }))
        ));
        client
            .insert(
                12,
                Object::Surface(SurfaceState {
                    role: Some(SurfaceRole::Xdg(9)),
                    ..SurfaceState::default()
                }),
            )
            .unwrap();
        let mut stale = wire::Builder::new();
        stale.u32(serial.wrapping_sub(1));
        stale.u32(12);
        stale.i32(0);
        stale.i32(0);
        client
            .dispatch(request(6, 0, stale).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut conflict = wire::Builder::new();
        conflict.u32(serial);
        conflict.u32(12);
        conflict.i32(0);
        conflict.i32(0);
        assert!(client
            .dispatch(request(6, 0, conflict).unwrap(), &mut VecDeque::new())
            .is_err());
        assert_eq!(client.protocol_error_code, WL_POINTER_ERROR_ROLE);

        let buffer = Buffer {
            serial: 1,
            file: Arc::new(File::open(&pool_path).unwrap()),
            offset: 0,
            width: 1,
            height: 1,
            stride: 4,
            format: SHM_XRGB8888,
        };
        client.insert(7, Object::Buffer(buffer.clone())).unwrap();
        let state = SurfaceState {
            pending_buffer: Some(PendingBuffer::Buffer { object: 7, buffer }),
            role: Some(SurfaceRole::Cursor),
            ..SurfaceState::default()
        };
        client.objects.insert(5, Object::Surface(state.clone()));
        client.commit_surface(5, state).unwrap();
        let release = receive_messages(&mut peer, 1);
        assert_eq!(
            (
                release.first().unwrap().object,
                release.first().unwrap().opcode
            ),
            (7, 0)
        );
        assert_eq!(
            runtime.lock().unwrap().surface_size(SurfaceKey {
                client: 2,
                object: 5,
            }),
            None
        );
        assert_eq!(client.mapped_total, 0);
        // The hotspot reached the scene rather than being parsed and dropped,
        // and the pixels came with it. Both halves matter and arrive by
        // different routes — the hotspot with the request, the image with the
        // commit — so this is what says the two met.
        assert_eq!(
            runtime.lock().unwrap().cursor_image(),
            Some((1, 2, 1, 1)),
            "the set_cursor hotspot and the committed image reach the scene"
        );

        // A null surface asks for NO cursor, and it drops the image the
        // client had already committed rather than leaving it standing.
        let mut hide = wire::Builder::new();
        hide.u32(serial);
        hide.u32(0);
        hide.i32(0);
        hide.i32(0);
        client
            .dispatch(request(6, 0, hide).unwrap(), &mut VecDeque::new())
            .unwrap();
        assert_eq!(runtime.lock().unwrap().cursor_image(), None);
        assert!(runtime.lock().unwrap().cursor_is_hidden());

        client.insert(8, Object::XdgWmBase).unwrap();
        let mut xdg = wire::Builder::new();
        xdg.u32(9);
        xdg.u32(5);
        assert!(client
            .dispatch(request(8, 2, xdg).unwrap(), &mut VecDeque::new())
            .unwrap_err()
            .contains("already has a role"));
        stop.stop();
        runtime.lock().unwrap().unsubscribe_keyboard(2);
        fs::remove_file(framebuffer_path).unwrap();
        fs::remove_file(pool_path).unwrap();
    }

    /// A cursor td refuses to hold pixels for must still have its buffer
    /// released. The client is entitled to reuse it, and one left waiting
    /// stops drawing altogether — a worse failure than the cursor it asked
    /// for not appearing.
    #[test]
    fn an_oversized_cursor_is_refused_with_its_buffer_still_released() {
        let stem = format!(
            "td-wayland-cursor-bound-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let pool_path = std::env::temp_dir().join(format!("{stem}.pool"));
        let over = crate::scene::MAX_CURSOR_DIMENSION.saturating_add(1);
        // FOUR BYTES behind a buffer that declares 257 pixels. The bound is
        // applied before `copy_buffer`, so nothing reads this file; a bound
        // applied afterwards — which bounds what is RETAINED and not what is
        // SPENT — would try to read 1028 bytes from it and fail the commit.
        // That failure is what pins the check to the right side of the copy.
        fs::write(&pool_path, [1, 2, 3, 0]).unwrap();
        let framebuffer = Framebuffer::test_file(
            &framebuffer_path,
            80,
            crate::scene::least_output_height(8),
            80 * 4,
        )
        .unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let focused = SurfaceKey {
            client: 2,
            object: 9,
        };
        runtime
            .lock()
            .unwrap()
            .commit(
                focused,
                Surface {
                    width: 32,
                    height: 32,
                    pixels: vec![1; 32 * 32 * 4],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        let rect = runtime
            .lock()
            .unwrap()
            .layout_snapshot()
            .get(&focused)
            .unwrap()
            .rect;
        runtime
            .lock()
            .unwrap()
            .pointer_frame(
                1,
                i32::try_from(rect.x.saturating_add(2)).unwrap(),
                i32::try_from(rect.y.saturating_add(3)).unwrap(),
                &[],
                PointerScroll::default(),
            )
            .unwrap();
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut client = Client::new(2, server, Arc::clone(&runtime), test_keymap()).unwrap();
        let subscription = runtime
            .lock()
            .unwrap()
            .subscribe_input_with_activity(
                2,
                Arc::clone(&client.keyboard_active),
                Arc::clone(&client.pointer_active),
            )
            .unwrap();
        let (_receiver, stop) = subscription.split();
        client
            .insert(5, Object::Surface(SurfaceState::default()))
            .unwrap();
        client.create_pointer(6, 7).unwrap();
        let enter = receive_messages(&mut peer, 2);
        let mut first = wire::Cursor::new(&enter.first().unwrap().payload);
        let serial = first.u32().unwrap();

        let mut set_cursor = wire::Builder::new();
        set_cursor.u32(serial);
        set_cursor.u32(5);
        set_cursor.i32(0);
        set_cursor.i32(0);
        client
            .dispatch(request(6, 0, set_cursor).unwrap(), &mut VecDeque::new())
            .unwrap();

        let buffer = Buffer {
            serial: 1,
            file: Arc::new(File::open(&pool_path).unwrap()),
            offset: 0,
            width: over,
            height: 1,
            stride: over.saturating_mul(4),
            format: SHM_XRGB8888,
        };
        client.insert(7, Object::Buffer(buffer.clone())).unwrap();
        let state = SurfaceState {
            pending_buffer: Some(PendingBuffer::Buffer { object: 7, buffer }),
            role: Some(SurfaceRole::Cursor),
            ..SurfaceState::default()
        };
        client.objects.insert(5, Object::Surface(state.clone()));
        // Not an error: the protocol permits the size, so refusing to DRAW it
        // is td's bound rather than the client's mistake.
        client.commit_surface(5, state).unwrap();
        let release = receive_messages(&mut peer, 1);
        assert_eq!(
            (
                release.first().unwrap().object,
                release.first().unwrap().opcode
            ),
            (7, 0)
        );
        assert_eq!(runtime.lock().unwrap().cursor_image(), None);
        assert!(!runtime.lock().unwrap().cursor_is_hidden());

        stop.stop();
        runtime.lock().unwrap().unsubscribe_keyboard(2);
        fs::remove_file(framebuffer_path).unwrap();
        fs::remove_file(pool_path).unwrap();
    }

    #[test]
    fn malformed_wire_input_still_removes_the_clients_scene() {
        let stem = format!(
            "td-wayland-cleanup-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let framebuffer = Framebuffer::test_file(&framebuffer_path, 8, 8, 32).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        runtime
            .lock()
            .unwrap()
            .commit(
                SurfaceKey {
                    client: 77,
                    object: 5,
                },
                Surface {
                    width: 1,
                    height: 1,
                    pixels: vec![1, 2, 3, 0],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        let (server, mut peer) = UnixStream::pair().unwrap();
        let thread_runtime = Arc::clone(&runtime);
        let keymap = test_keymap();
        let worker = thread::spawn(move || serve_client(server, 77, thread_runtime, keymap));
        let mut malformed = Vec::new();
        malformed.extend_from_slice(&1u32.to_ne_bytes());
        malformed.extend_from_slice(&(7u32 << 16).to_ne_bytes());
        peer.write_all(&malformed).unwrap();
        drop(peer);
        assert!(worker.join().unwrap().is_err());
        let frame = fs::read(&framebuffer_path).unwrap();
        assert!(!frame.as_chunks::<4>().0.contains(&[1, 2, 3, 0]));
        fs::remove_file(framebuffer_path).unwrap();
    }

    #[test]
    fn client_memory_and_connection_limits_fail_closed() {
        assert_eq!(
            client_surface_total(MAX_UI_FRAME_BYTES, 4096, 4096).unwrap(),
            MAX_UI_FRAME_BYTES
        );
        assert!(client_surface_total(MAX_UI_FRAME_BYTES, 0, 1).is_err());
        let mut permits = Vec::new();
        for _ in 0..MAX_CLIENTS {
            permits.push(ClientPermit::acquire().unwrap());
        }
        assert!(ClientPermit::acquire().is_err());
        drop(permits);
        assert_eq!(ACTIVE_CLIENTS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn message_builder_helper_is_not_vacuous() {
        let mut builder = wire::Builder::new();
        builder.u32(2);
        let message = request(1, 1, builder).unwrap();
        assert_eq!(message.object, 1);
        assert_eq!(message.opcode, 1);
    }

    #[test]
    fn an_axis_carries_its_source_and_notch_count_only_from_version_five() {
        use crate::pointer::{AxisStep, PointerAxis};
        // Decoded from the BYTES rather than read off the builder, so the
        // header the client will parse is what these assertions are about.
        let parse_message = |encoded: &Vec<u8>| {
            let mut bytes = encoded.clone();
            wire::take(&mut bytes).unwrap().unwrap()
        };
        let surface = SurfaceKey {
            client: 1,
            object: 9,
        };
        let event = PointerEvent::Axis {
            surface,
            time: 44,
            step: AxisStep {
                axis: PointerAxis::Vertical,
                value: -20,
                detents: -2,
            },
        };

        // Version 4 gets the axis and nothing else — even ASKED for the
        // source. `axis_source` and `axis_discrete` do not exist for it, and
        // sending one would be a message its object cannot decode.
        let four = pointer_messages(7, 4, None, &event, true).unwrap();
        assert_eq!(four.len(), 1);
        let axis = parse_message(four.first().unwrap());
        assert_eq!(axis.opcode, 4);
        let mut body = wire::Cursor::new(&axis.payload);
        assert_eq!(body.u32().unwrap(), 44);
        assert_eq!(body.u32().unwrap(), 0, "vertical is axis 0");
        assert_eq!(body.i32().unwrap(), -20 * 256, "the value is wl_fixed");
        body.finish().unwrap();

        // Version 5 gets all three when this is the frame's FIRST axis, and
        // the two qualifiers come first: a client reading them in order
        // learns what kind of scroll it is before it is handed a distance.
        let five = pointer_messages(7, 5, None, &event, true).unwrap();
        assert_eq!(five.len(), 3);
        let source = parse_message(five.first().unwrap());
        assert_eq!(source.opcode, 6);
        let mut body = wire::Cursor::new(&source.payload);
        assert_eq!(body.u32().unwrap(), 0, "the source is a wheel");
        body.finish().unwrap();

        let discrete = parse_message(five.get(1).unwrap());
        assert_eq!(discrete.opcode, 8);
        let mut body = wire::Cursor::new(&discrete.payload);
        assert_eq!(body.u32().unwrap(), 0);
        // Sign, not just magnitude: the protocol requires the discrete count
        // to agree with the value beside it, and a client that scrolled by
        // the notch count would go the wrong way on a disagreement.
        assert_eq!(body.i32().unwrap(), -2);
        body.finish().unwrap();
        assert_eq!(parse_message(five.get(2).unwrap()).opcode, 4);

        // Every other event is one message whatever the version, so the
        // branch above is the axis's alone.
        let motion = PointerEvent::Motion {
            time: 1,
            target: PointerTarget {
                surface,
                x: 0,
                y: 0,
            },
        };
        assert_eq!(
            pointer_messages(7, 5, None, &motion, true).unwrap().len(),
            1
        );
        assert_eq!(
            pointer_messages(7, 4, None, &motion, true).unwrap().len(),
            1
        );

        // A second axis in the same frame is asked for no source, and that is
        // the whole of the difference: the notch count still rides with it,
        // being one per axis rather than one per frame.
        let second = pointer_messages(7, 5, None, &event, false).unwrap();
        assert_eq!(second.len(), 2);
        assert_eq!(parse_message(second.first().unwrap()).opcode, 8);
        assert_eq!(parse_message(second.get(1).unwrap()).opcode, 4);

        // And version 8 would get no notch count at all — it is deprecated
        // there in favour of `axis_value120`. Unreachable while `wl_seat` is
        // advertised at 7, which the const assertion beside `SEAT_VERSION`
        // holds it to; asserted here because the encoder is what would send
        // it, and a bound only its caller respects is not one.
        let eight = pointer_messages(7, 8, None, &event, true).unwrap();
        assert_eq!(eight.len(), 2);
        assert_eq!(parse_message(eight.first().unwrap()).opcode, 6);
        assert_eq!(parse_message(eight.get(1).unwrap()).opcode, 4);
    }

    #[test]
    fn pointer_fixed_encoding_is_exact_and_checked() {
        assert_eq!(pointer_fixed(0).unwrap(), 0);
        assert_eq!(pointer_fixed(17).unwrap(), 17 * 256);
        assert_eq!(pointer_fixed(-9).unwrap(), -9 * 256);
        assert!(pointer_fixed(i32::MAX).is_err());
        assert!(pointer_fixed(i32::MIN).is_err());
    }
}
