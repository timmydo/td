use crate::configure::{Configure, ConfigureTracker, ToplevelState, ViewStatus};
use crate::keyboard::{KeyboardEvent, KeyboardSnapshot, XKB_KEYMAP};
use crate::layout::ViewLayout;
use crate::pointer::{PointerEvent, PointerSnapshot, RoutedPointerFrame};
use crate::positioner::{Anchor, Gravity, Positioner, Rect as PositionerRect};
use crate::runtime::{KeyboardDelivery, KeyboardSubscriptionStop, Runtime, SubscriptionStop};
use crate::scene::{
    CursorRequest, InputRegion, PopupPlacement, SharedInputRegion, Surface, SurfaceKey,
    WindowGeometry, MAX_CURSOR_DIMENSION, MAX_INPUT_REGION_OPERATIONS, SHM_ARGB8888, SHM_XRGB8888,
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
/// Three of `xdg_surface.error`, each named by the protocol for exactly the
/// mistake it is raised for: a request to an xdg_surface that has no role
/// object yet, a second role object on one that already has one, and a
/// `set_window_geometry` whose width or height is not positive.
/// Raised in place of the `implementation` code every other refusal here
/// carries, because these are mistakes the protocol has words for — and the
/// word is what tells a toolkit author which of them it made. The enum
/// carries no `since`, so neither code is gated on the `xdg_wm_base` version a
/// client bound; a client that does not recognise the number still reads the
/// message beside it, which spells the same thing out.
const XDG_SURFACE_ERROR_NOT_CONSTRUCTED: u32 = 1;
const XDG_SURFACE_ERROR_ALREADY_CONSTRUCTED: u32 = 2;
const XDG_SURFACE_ERROR_INVALID_SIZE: u32 = 5;
/// `invalid_input` is the positioner's own error and is raised ON the
/// positioner, since that is the object the client got wrong. The five
/// `xdg_wm_base` codes belong to the SHELL object and are raised on it
/// wherever the request that broke the rule arrived — which for all but
/// `defunct_surfaces` is some other object.
const XDG_POSITIONER_ERROR_INVALID_INPUT: u32 = 0;
const XDG_WM_BASE_ERROR_ROLE: u32 = 0;
const XDG_WM_BASE_ERROR_DEFUNCT_SURFACES: u32 = 1;
const XDG_WM_BASE_ERROR_NOT_THE_TOPMOST_POPUP: u32 = 2;
const XDG_WM_BASE_ERROR_INVALID_POPUP_PARENT: u32 = 3;
const XDG_WM_BASE_ERROR_INVALID_POSITIONER: u32 = 5;
const XDG_POPUP_CONFIGURE: u16 = 0;
const XDG_POPUP_POPUP_DONE: u16 = 1;
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

/// An `attach` that no commit has applied yet. The OFFSET rides in here rather
/// than beside it because that is where the protocol puts it: at the version td
/// advertises there is no way to send one without an attach, and a second field
/// on the surface would be a second place for it to go stale.
#[derive(Clone)]
enum PendingBuffer {
    Detach {
        offset: (i32, i32),
    },
    Buffer {
        object: u32,
        buffer: Buffer,
        offset: (i32, i32),
    },
}

/// The role object an xdg_surface has been given. It is the whole of what
/// "constructed" means for one — the protocol refuses every other request
/// until a role is assigned — and the two arms are placed by entirely
/// different rules: a toplevel is tiled by td, a popup is placed by the
/// client's own positioner. One field rather than two `Option`s, so "has a
/// role object" is one question with one answer.
#[derive(Clone, Copy, Eq, PartialEq)]
enum RoleObject {
    Toplevel(u32),
    Popup(u32),
}

/// WHICH role an xdg_surface was given, without the object that carried it.
/// A role is permanent — the protocol says so of a wl_surface's — and the role
/// object is not: destroying an xdg_toplevel hands the xdg_surface back, and a
/// client may build another. What it may not do is build a DIFFERENT one, so
/// this outlives the object and `role_object` cannot answer it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RoleKind {
    Toplevel,
    Popup,
}

impl RoleKind {
    fn interface(self) -> &'static str {
        match self {
            RoleKind::Toplevel => "xdg_toplevel",
            RoleKind::Popup => "xdg_popup",
        }
    }
}

impl RoleObject {
    /// What a diagnostic calls it. The protocol names the interface in its own
    /// errors, and a client told "before its xdg_toplevel" when it destroyed a
    /// popup has been told about the wrong object.
    fn interface(self) -> &'static str {
        self.kind().interface()
    }

    fn kind(self) -> RoleKind {
        match self {
            RoleObject::Toplevel(_) => RoleKind::Toplevel,
            RoleObject::Popup(_) => RoleKind::Popup,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SurfaceRole {
    Xdg(u32),
    /// The xdg_surface this role named is destroyed. The ROLE stays, since a
    /// wl_surface takes one ever; the ID does not, because the client has that
    /// number back and may put anything behind it.
    XdgRetired,
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
        /// The `xdg_wm_base` this came from. Carried because two of the
        /// protocol errors an xdg_surface request can raise are the SHELL
        /// object's, and an error is posted against an object: codes 3 and 5
        /// on an xdg_surface are `unconfigured_buffer` and `invalid_size`, so
        /// a client decoding one against the object it arrived on is told
        /// something else entirely.
        wm_base: u32,
        role_object: Option<RoleObject>,
        /// Which role this xdg_surface has EVER been given. A role object may
        /// be destroyed and replaced by another of the same kind — a client
        /// reusing a window it already measured — but the role itself is
        /// permanent, so a surface that was a popup may not come back as a
        /// toplevel and be tiled.
        assigned: Option<RoleKind>,
        configure: Arc<Mutex<ConfigureTracker>>,
        /// The window geometry a client has asked for and no commit has applied
        /// yet. Double-buffered because the protocol says so, and held on the
        /// xdg_surface for the decoration's reason: this is the object whose
        /// state it is, so it dies with the right one and there is no second
        /// place to leave it stale. Only the PENDING half lives here — what
        /// took effect is the scene's, which is what draws and hit-tests with
        /// it.
        pending_geometry: Option<WindowGeometry>,
    },
    /// The rules for a popup that has not been placed yet. Plain data: the
    /// protocol says a compositor COPIES them at `get_popup`, so this object is
    /// free to be reused or destroyed straight afterwards and nothing that has
    /// already been placed may look at it again.
    Positioner(Positioner),
    XdgPopup {
        xdg_surface: u32,
        /// The PARENT's wl_surface, which is what the scene needs — a placement
        /// is measured from that surface's window geometry, and the parent's
        /// own xdg_surface is only how the client named it.
        ///
        /// `None` once that surface is destroyed, and the option is the whole
        /// point rather than tidiness: an id is not an identity. Wayland
        /// recycles them — td retires them with `wl_display.delete_id`
        /// precisely so a client may — so an edge left holding the number
        /// would come to name whatever took it next, which is a menu placed
        /// on a window that never opened it.
        parent: Option<u32>,
        /// Where the rules put it, resolved once at `get_popup` and copied here
        /// for the reason above.
        rect: PositionerRect,
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

/// A popup's initial configure: where it was placed and how big, then the
/// xdg_surface serial that makes the pair one atomic configuration. The
/// rectangle rather than a size alone is what distinguishes this from a
/// toplevel's — a popup is placed by its client's own rules, and the client is
/// told the answer it will be drawn at.
fn send_popup_configure(
    outbound: &Arc<Mutex<Outbound>>,
    xdg_surface: u32,
    popup: u32,
    rect: PositionerRect,
    tracker: &Arc<Mutex<ConfigureTracker>>,
) -> Result<(), String> {
    let configure = tracker
        .lock()
        .map_err(|_| "XDG configure tracker lock poisoned".to_string())?
        .initial(next_serial())?;
    let mut placed = wire::Builder::new();
    placed.i32(rect.x);
    placed.i32(rect.y);
    placed.i32(rect.width);
    placed.i32(rect.height);
    let placed = placed.message(popup, XDG_POPUP_CONFIGURE)?;

    let mut surface = wire::Builder::new();
    surface.u32(configure.serial);
    let surface = surface.message(xdg_surface, 0)?;

    let mut outbound = outbound
        .lock()
        .map_err(|_| "Wayland outbound lock poisoned".to_string())?;
    outbound.send(&placed)?;
    outbound.send(&surface)
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
        let dropped = self
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .unmap(SurfaceKey {
                client: self.id,
                object: surface,
            })?;
        self.release_dropped(&dropped)?;
        Ok(())
    }

    fn remove_surface(&mut self, surface: u32) -> Result<(), String> {
        self.clear_surface_bytes(surface);
        let dropped = self
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .remove(SurfaceKey {
                client: self.id,
                object: surface,
            })?;
        // Reached by a menu repainted after its window's role object went: the
        // popup object outlives that, so its placement goes back into the
        // scene and only this gives the bytes back.
        self.release_dropped(&dropped)?;
        Ok(())
    }

    /// Everything owed to the menus a take-down cascaded over: their bytes
    /// back, and their clients TOLD. One call rather than two beside each
    /// other at three sites, because they are one event seen twice — a
    /// take-down that refunded and forgot to dismiss is the shape of bug the
    /// refund itself was.
    ///
    /// The scene names them, since its cascade is what decides how far a
    /// take-down reaches; this side only has to agree with it.
    ///
    /// The client check is not ceremony, though nothing today can fail it:
    /// both halves are spelled by OBJECT id, and object ids are per
    /// connection, so a key from another client would refund — and send an
    /// event to — whichever object of this one happens to share its number.
    ///
    /// A take-down whose paint fails never reaches this, and does not have to:
    /// the error ends the connection, so the ledger it would correct is the
    /// departing `Client`'s own field and the events would go to a socket
    /// being shut down.
    fn release_dropped(&mut self, dropped: &[SurfaceKey]) -> Result<(), String> {
        for key in dropped {
            if key.client == self.id {
                self.clear_surface_bytes(key.object);
            }
        }
        // BACKWARDS, which is the whole of the ordering requirement: the
        // protocol dismisses nested popups in the order it makes a client
        // destroy them, topmost first, and the cascade is breadth-first from
        // the surface that went — so a parent is always at a lower index than
        // its submenus and reversing puts every child ahead of its parent.
        //
        // The refund above is a SEPARATE loop that has already run to
        // completion, which is what stops a send failing part way down here
        // from costing a client bytes. Its own direction is immaterial.
        for key in dropped.iter().rev() {
            if key.client != self.id {
                continue;
            }
            if let Some(popup) = self.popup_object_of(key.object) {
                self.send(popup, XDG_POPUP_POPUP_DONE, wire::Builder::new())?;
            }
        }
        Ok(())
    }

    /// The `xdg_popup` standing for a wl_surface, if one does — walked
    /// FORWARDS along the two edges the surface already keeps, which is the
    /// same pair `commit_surface` follows to reach this object. Asking the
    /// surface which role object is current answers with the current one; a
    /// scan for any popup that happens to name the surface answers with
    /// whichever the object map yields first, and the two are the same only
    /// while nothing has been replaced.
    fn popup_object_of(&self, surface: u32) -> Option<u32> {
        let Some(Object::Surface(state)) = self.objects.get(&surface) else {
            return None;
        };
        let Some(SurfaceRole::Xdg(role)) = state.role else {
            return None;
        };
        match self.objects.get(&role) {
            Some(Object::XdgSurface {
                role_object: Some(RoleObject::Popup(popup)),
                ..
            }) => Some(*popup),
            _ => None,
        }
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

    /// Take a popup off the screen. Named by its wl_surface, which is what the
    /// scene knows it by.
    /// Clears the surface's bytes as `unmap_surface` does. A popup's pixels
    /// are the client's ceiling like any other, and a menu that gave them back
    /// to the scene and not to the accounting would have a client opening and
    /// dismissing menus disconnected for buffers td is not holding.
    fn unmap_popup(&mut self, surface: u32) -> Result<(), String> {
        self.clear_surface_bytes(surface);
        let dropped = self
            .runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .unmap_popup(SurfaceKey {
                client: self.id,
                object: surface,
            })?;
        self.release_dropped(&dropped)?;
        Ok(())
    }

    /// Break the parent edge of every popup that named this wl_surface. Only
    /// the surfaces that named it: a submenu's own edge points at the ORPHAN,
    /// which still exists, so it keeps that edge and never resolves to a rect
    /// — the walk ends at a surface no layout has. Its pixels are not left
    /// charged: this edits the object map alone, and the `remove_surface` that
    /// follows cascades in the scene and refunds what it drops.
    fn orphan_popups_of(&mut self, surface: u32) {
        for object in self.objects.values_mut() {
            if let Object::XdgPopup { parent, .. } = object {
                if *parent == Some(surface) {
                    *parent = None;
                }
            }
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
        if state.role == Some(SurfaceRole::XdgRetired) {
            return Err(format!(
                "wl_surface {id} was committed after its xdg_surface was destroyed"
            ));
        }
        let mut xdg_configure = None;
        let mut geometry = None;
        // Where this surface goes if it is a POPUP, which is what tells the
        // buffer path below to float it over its parent rather than tile it.
        let mut popup: Option<PopupPlacement> = None;
        let mut is_popup = false;
        if let Some(SurfaceRole::Xdg(role)) = state.role {
            let xdg = self
                .objects
                .get(&role)
                .cloned()
                .ok_or_else(|| format!("wl_surface {id} has a destroyed xdg_surface role"))?;
            let Object::XdgSurface {
                role_object,
                configure,
                ..
            } = xdg
            else {
                return Err(format!("wl_surface {id} has a non-XDG role object"));
            };
            let role_object =
                role_object.ok_or_else(|| format!("xdg_surface {role} has no role object"))?;
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
                match role_object {
                    RoleObject::Toplevel(toplevel) => send_initial_configure(
                        &self.outbound,
                        &ConfigureRegistration {
                            xdg_surface: role,
                            toplevel,
                            tracker: Arc::clone(&configure),
                        },
                    )?,
                    // A popup is told WHERE as well as how big, and is not
                    // registered for layout updates: it is in no arrangement,
                    // so the publisher that answers "what tile do you have
                    // now" has nothing to say about one.
                    RoleObject::Popup(popup_object) => {
                        let Some(Object::XdgPopup { rect, .. }) =
                            self.objects.get(&popup_object).cloned()
                        else {
                            return Err(format!(
                                "xdg_surface {role} names a missing xdg_popup {popup_object}"
                            ));
                        };
                        send_popup_configure(&self.outbound, role, popup_object, rect, &configure)?;
                    }
                }
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
            if let RoleObject::Popup(popup_object) = role_object {
                let Some(Object::XdgPopup { parent, rect, .. }) =
                    self.objects.get(&popup_object).cloned()
                else {
                    return Err(format!(
                        "xdg_surface {role} names a missing xdg_popup {popup_object}"
                    ));
                };
                // Tracked apart from the placement, because a popup with no
                // parent still has to take the POPUP path: `popup` is what
                // every branch below asks whether this surface is one, so an
                // orphan that answered `None` would fall through to the tile
                // arm and join the arrangement — a menu given a share of the
                // screen, a title band and a drag handle.
                is_popup = true;
                popup = parent.map(|parent| PopupPlacement {
                    parent: SurfaceKey {
                        client: self.id,
                        object: parent,
                    },
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                });
            }
            // Taken after the refusals above and pushed after the pixels below,
            // so it is spent only by a commit that is going to happen and
            // applied only once the pixels it describes are the scene's. A crop
            // applied before the buffer it was measured against would draw one
            // frame of the previous buffer through the new window's bounds.
            //
            // An ORPHAN is the one commit that happens and applies nothing, so
            // it does not spend one either: `get_popup` carries a pending
            // geometry across a rebuilt role object, and a menu rebuilt on this
            // xdg_surface is entitled to the size its client last set.
            if !(is_popup && popup.is_none()) {
                if let Some(Object::XdgSurface {
                    pending_geometry, ..
                }) = self.objects.get_mut(&role)
                {
                    geometry = pending_geometry.take();
                }
            }
        } else if state.role.is_none() && attaching_buffer {
            return Err(format!("wl_surface {id} attached a buffer without a role"));
        }
        if let Some(pending) = state.pending_buffer {
            let key = SurfaceKey {
                client: self.id,
                object: id,
            };
            // What the attach asked to move by. It travels WITH the contents
            // rather than in a call of its own, so one commit is one repaint —
            // and because for a cursor the two are the same statement: the
            // offset moves the image, and moving the hotspot is how a cursor
            // says that.
            let offset = match &pending {
                PendingBuffer::Detach { offset } => *offset,
                PendingBuffer::Buffer { offset, .. } => *offset,
            };
            if cursor {
                match pending {
                    // A cursor surface with its buffer taken away has no
                    // image to be, so td's own cross stands. Deliberately NOT
                    // read as "hide": a client asking for no cursor says so
                    // with a null SURFACE, which cannot be confused with a
                    // client between two frames of an animated one.
                    PendingBuffer::Detach { .. } => self
                        .runtime
                        .lock()
                        .map_err(|_| "runtime lock poisoned".to_string())?
                        .detach_cursor(key, offset)?,
                    PendingBuffer::Buffer { object, buffer, .. } => {
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
                                .commit_cursor(key, image, offset)?;
                        } else {
                            // Still a REPLACEMENT: the surface's contents are
                            // now something td will not draw, so the frame it
                            // held is one the client has superseded.
                            self.runtime
                                .lock()
                                .map_err(|_| "runtime lock poisoned".to_string())?
                                .detach_cursor(key, offset)?;
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
            } else if is_popup {
                // A popup is neither tiled nor counted against the layout: it
                // floats over its parent where the client's own rules put it.
                // Its BYTES are still the client's, since the ceiling is about
                // what one connection can make td hold rather than about
                // windows.
                match (pending, popup) {
                    (PendingBuffer::Detach { .. }, _) => {
                        if was_mapped {
                            if let Some(configure) = xdg_configure {
                                configure
                                    .lock()
                                    .map_err(|_| "XDG configure tracker lock poisoned".to_string())?
                                    .unmap()?;
                            }
                        }
                        self.unmap_popup(id)?;
                    }
                    // The parent surface is gone, so there is nowhere to put
                    // this. Taken DOWN rather than left as it was: the menu is
                    // no longer on screen and its pixels are no longer the
                    // client's to be charged for. Not an error either — the
                    // client broke nothing by closing a window with a menu up.
                    // It IS told, though. What this arm is FOR is the one
                    // dismissal no cascade can reach — a popup still unmapped
                    // when its window went was never in the scene to be
                    // cascaded over, so this commit of a buffer is the only
                    // moment td can ever decide against it. What it FIRES on
                    // is wider: a popup that was mapped, was dismissed by the
                    // cascade, and is repainted anyway hits it too and hears
                    // `popup_done` a second time. That is redundant rather
                    // than ill-formed — the event carries no argument and no
                    // serial — and it is only reachable by a client that
                    // ignored the first one.
                    (PendingBuffer::Buffer { object, buffer, .. }, None) => {
                        // The configure tracker is left MAPPED, unlike the
                        // detach above. Unmapping it clears `initial_sent`, and
                        // a buffer on a surface without one is a protocol
                        // error — so a client repainting its abandoned menu a
                        // second time would be disconnected for it. Clearing
                        // the bytes leaves `was_mapped` false from here on, so
                        // a later detach does not reset it either — and what
                        // makes that unreadable is not that the surface is
                        // finished (destroy this popup and build another on the
                        // same xdg_surface and it maps) but that destroying the
                        // popup replaces the tracker.
                        self.unmap_popup(id)?;
                        // AFTER the submenus `unmap_popup` just dismissed,
                        // which hang above this one: the protocol's order is
                        // topmost first whichever call the popups came from.
                        if let Some(popup) = self.popup_object_of(id) {
                            self.send(popup, XDG_POPUP_POPUP_DONE, wire::Builder::new())?;
                        }
                        if matches!(
                            self.objects.get(&object),
                            Some(Object::Buffer(current)) if current.serial == buffer.serial
                        ) {
                            self.send(object, 0, wire::Builder::new())?;
                        }
                    }
                    (PendingBuffer::Buffer { object, buffer, .. }, Some(placement)) => {
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
                            .commit_popup(
                                key,
                                Some(surface),
                                placement,
                                Some(input_region.clone()),
                                geometry,
                            )?;
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
            } else {
                // A TILE ignores the offset, and that is the answer rather
                // than an omission: the tile fixes where the window is and the
                // geometry names which part of the buffer fills it, so shifting
                // the contents on top would move a window inside its own tile
                // and leave a gap at the edge it moved from.
                match pending {
                    PendingBuffer::Detach { .. } => {
                        if was_mapped {
                            if let Some(configure) = xdg_configure {
                                configure
                                    .lock()
                                    .map_err(|_| "XDG configure tracker lock poisoned".to_string())?
                                    .unmap()?;
                            }
                        }
                        self.unmap_surface(id)?;
                        // The one commit shape where the geometry is applied on
                        // its own, and it needs no atomicity: a surface with no
                        // pixels is drawn nowhere and aimed at by nothing, so
                        // the unmap and the crop cannot be told apart from
                        // outside whichever order they land in.
                        if geometry.is_some() {
                            self.runtime
                                .lock()
                                .map_err(|_| "runtime lock poisoned".to_string())?
                                .set_window_geometry(key, geometry)?;
                        }
                    }
                    PendingBuffer::Buffer { object, buffer, .. } => {
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
                            .apply_commit(
                                key,
                                Some(surface),
                                Some(input_region.clone()),
                                geometry,
                            )?;
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
        } else if let (Some(placement), true) = (popup, input_region_changed || geometry.is_some())
        {
            self.runtime
                .lock()
                .map_err(|_| "runtime lock poisoned".to_string())?
                .commit_popup(
                    SurfaceKey {
                        client: self.id,
                        object: id,
                    },
                    None,
                    placement,
                    input_region_changed.then(|| input_region.clone()),
                    geometry,
                )?;
        } else if (input_region_changed || geometry.is_some()) && !cursor && !is_popup {
            // No buffer, so the state this commit carries is whichever of the
            // two arrived — the geometry alone being the ordinary opening
            // sequence, set on the empty commit before the first frame.
            self.runtime
                .lock()
                .map_err(|_| "runtime lock poisoned".to_string())?
                .apply_commit(
                    SurfaceKey {
                        client: self.id,
                        object: id,
                    },
                    None,
                    input_region_changed.then(|| input_region.clone()),
                    geometry,
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
                    // `Xdg` names a LIVE xdg_surface — destroying one retires
                    // the role — so this asks the role rather than looking the
                    // id up, and a client that tore its shell objects down in
                    // the right order is not refused by a number it reused.
                    if matches!(state.role, Some(SurfaceRole::Xdg(_))) {
                        return Err(format!(
                            "wl_surface {} was destroyed before its role object",
                            message.object
                        ));
                    }
                    // Every popup that named this surface as its parent loses
                    // that edge HERE, while the id still means what it meant.
                    // A moment later it is back in the client's pool.
                    self.orphan_popups_of(message.object);
                    self.remove_surface(message.object)?;
                    self.remove_surface_object(message.object)
                }
                1 => {
                    let buffer = args.u32()?;
                    // Where the new buffer's corner lands relative to the one it
                    // replaces. Kept rather than dropped for the CURSOR, whose
                    // hotspot the protocol moves by it; a tile ignores it, and
                    // `commit_surface` is where that division is made. Reading
                    // it at all depends on the version advertised above: from
                    // wl_surface 5 a non-zero pair is `invalid_offset` and the
                    // arguments are ignored, so raising that cap means
                    // REFUSING here rather than honouring.
                    let offset = (args.i32()?, args.i32()?);
                    args.finish()?;
                    state.pending_buffer = if buffer == 0 {
                        Some(PendingBuffer::Detach { offset })
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
                            offset,
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
                        Some(SurfaceRole::Xdg(_) | SurfaceRole::XdgRetired) => {
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
                    // The shell object outlives what it made. That is the
                    // protocol's rule, and it is also what keeps the id each
                    // xdg_surface carries meaningful: destroyed here, it would
                    // be recycled, and an error raised later against "the
                    // xdg_wm_base" would name whatever took the number.
                    let child = self.objects.iter().find_map(|(id, object)| match object {
                        Object::XdgSurface { wm_base, .. } if *wm_base == message.object => {
                            Some(*id)
                        }
                        _ => None,
                    });
                    if let Some(child) = child {
                        return self.fail_protocol(
                            XDG_WM_BASE_ERROR_DEFUNCT_SURFACES,
                            &format!(
                                "xdg_wm_base {} is destroyed before xdg_surface {child}",
                                message.object
                            ),
                        );
                    }
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
                            wm_base: message.object,
                            role_object: None,
                            assigned: None,
                            configure: Arc::new(Mutex::new(ConfigureTracker::new())),
                            pending_geometry: None,
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
                // create_positioner. Empty: a positioner is INCOMPLETE until
                // the client sets a size and an anchor rectangle, and
                // `get_popup` is where that is checked.
                1 => {
                    let id = args.u32()?;
                    args.finish()?;
                    self.insert(id, Object::Positioner(Positioner::default()))
                }
                _ => Err(format!(
                    "unsupported xdg_wm_base request {}",
                    message.opcode
                )),
            },
            Object::XdgSurface {
                surface,
                wm_base,
                role_object,
                assigned,
                configure,
                pending_geometry,
            } => match message.opcode {
                0 => {
                    args.finish()?;
                    if let Some(role_object) = role_object {
                        return Err(format!(
                            "xdg_surface {} was destroyed before its {}",
                            message.object,
                            role_object.interface()
                        ));
                    }
                    self.remove_object(message.object)?;
                    // The wl_surface outlives its xdg_surface and keeps the
                    // role, so it can take neither a second one nor another
                    // commit. What it must not keep is the NUMBER: that id is
                    // back in the client's pool, and a role still holding it
                    // would answer with whatever is put there next.
                    let Some(Object::Surface(state)) = self.objects.get_mut(&surface) else {
                        return Err(format!(
                            "xdg_surface {} lost wl_surface {surface}",
                            message.object
                        ));
                    };
                    state.role = Some(SurfaceRole::XdgRetired);
                    // The geometry was THIS object's, so it goes with it — the
                    // crop's ownership rather than a stale one anything could
                    // still draw through. Unconditional, because the scene
                    // answers whether there was one.
                    self.runtime
                        .lock()
                        .map_err(|_| "runtime lock poisoned".to_string())?
                        .set_window_geometry(
                            SurfaceKey {
                                client: self.id,
                                object: surface,
                            },
                            None,
                        )
                }
                1 => {
                    let new_toplevel = args.u32()?;
                    args.finish()?;
                    if let Some(existing) = role_object {
                        return self.fail_protocol(
                            XDG_SURFACE_ERROR_ALREADY_CONSTRUCTED,
                            &format!(
                                "xdg_surface {} already has an {}",
                                message.object,
                                existing.interface()
                            ),
                        );
                    }
                    if let Some(had) = assigned.filter(|had| *had != RoleKind::Toplevel) {
                        return self.fail_protocol_on(
                            wm_base,
                            XDG_WM_BASE_ERROR_ROLE,
                            &format!(
                                "xdg_surface {} was {} and may not become an xdg_toplevel",
                                message.object,
                                had.interface()
                            ),
                        );
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
                            wm_base,
                            role_object: Some(RoleObject::Toplevel(new_toplevel)),
                            assigned: Some(RoleKind::Toplevel),
                            configure,
                            // Necessarily None — a geometry before this request
                            // is refused — and carried rather than written as
                            // None so this write-back states nothing of its own
                            // about state it does not touch.
                            pending_geometry,
                        },
                    );
                    Ok(())
                }
                // set_window_geometry. Recorded and applied by the next commit
                // to the wl_surface, which is what "double-buffered state"
                // means; nothing is answered, since the geometry is a statement
                // about the client's own buffer rather than a request.
                3 => {
                    let x = args.i32()?;
                    let y = args.i32()?;
                    let geometry_width = args.i32()?;
                    let geometry_height = args.i32()?;
                    args.finish()?;
                    // "A role must be assigned before any other requests are
                    // made to the xdg_surface object" — so a geometry set before
                    // `get_toplevel` is `not_constructed` rather than state to
                    // hold for the role that has not been asked for. Checked
                    // before the arguments are judged, because a client with no
                    // role has told td nothing about a window yet and that is
                    // the more useful half of the diagnosis.
                    if role_object.is_none() {
                        return self.fail_protocol(
                            XDG_SURFACE_ERROR_NOT_CONSTRUCTED,
                            &format!(
                                "xdg_surface {} set a window geometry before its role object",
                                message.object
                            ),
                        );
                    }
                    if geometry_width <= 0 || geometry_height <= 0 {
                        return self.fail_protocol(
                            XDG_SURFACE_ERROR_INVALID_SIZE,
                            &format!(
                                "xdg_surface {} window geometry {geometry_width}x{geometry_height} is not positive",
                                message.object
                            ),
                        );
                    }
                    let Some(Object::XdgSurface {
                        pending_geometry, ..
                    }) = self.objects.get_mut(&message.object)
                    else {
                        return Err(format!("xdg_surface {} went away", message.object));
                    };
                    *pending_geometry = Some(WindowGeometry {
                        x,
                        y,
                        width: geometry_width,
                        height: geometry_height,
                    });
                    Ok(())
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
                // get_popup(id, parent, positioner)
                2 => {
                    let id = args.u32()?;
                    let parent = args.u32()?;
                    let positioner = args.u32()?;
                    args.finish()?;
                    if let Some(existing) = role_object {
                        return self.fail_protocol(
                            XDG_SURFACE_ERROR_ALREADY_CONSTRUCTED,
                            &format!(
                                "xdg_surface {} already has an {}",
                                message.object,
                                existing.interface()
                            ),
                        );
                    }
                    if let Some(had) = assigned.filter(|had| *had != RoleKind::Popup) {
                        return self.fail_protocol_on(
                            wm_base,
                            XDG_WM_BASE_ERROR_ROLE,
                            &format!(
                                "xdg_surface {} was {} and may not become an xdg_popup",
                                message.object,
                                had.interface()
                            ),
                        );
                    }
                    if !matches!(self.objects.get(&surface), Some(Object::Surface(_))) {
                        return Err(format!(
                            "xdg_surface refers to missing wl_surface {surface}"
                        ));
                    }
                    // A null parent is allowed by the protocol only so that
                    // another protocol can supply one before the first commit.
                    // td implements no such protocol, so a popup that arrived
                    // this way could never be placed.
                    let Some(Object::XdgSurface {
                        surface: parent_surface,
                        role_object: parent_role,
                        ..
                    }) = self.objects.get(&parent).cloned()
                    else {
                        return self.fail_protocol_on(
                            wm_base,
                            XDG_WM_BASE_ERROR_INVALID_POPUP_PARENT,
                            &format!(
                                "xdg_popup {id} names {parent}, which is no xdg_surface of this client"
                            ),
                        );
                    };
                    // "The parent of an xdg_popup must be mapped before the
                    // xdg_popup itself" — and a parent with no role object at
                    // all cannot have been.
                    if parent_role.is_none() {
                        return self.fail_protocol_on(
                            wm_base,
                            XDG_WM_BASE_ERROR_INVALID_POPUP_PARENT,
                            &format!("xdg_popup {id} names unconstructed xdg_surface {parent}"),
                        );
                    }
                    let Some(Object::Positioner(rules)) = self.objects.get(&positioner).cloned()
                    else {
                        return Err(format!(
                            "xdg_popup {id} names {positioner}, which is no xdg_positioner"
                        ));
                    };
                    // The rules are COPIED here, as the protocol requires: the
                    // client may reuse or destroy the positioner immediately,
                    // and nothing placed by it may move afterwards.
                    let Some(rect) = rules.resolve() else {
                        return self.fail_protocol_on(
                            wm_base,
                            XDG_WM_BASE_ERROR_INVALID_POSITIONER,
                            &format!(
                                "xdg_popup {id} was given an incomplete xdg_positioner {positioner}"
                            ),
                        );
                    };
                    self.insert(
                        id,
                        Object::XdgPopup {
                            xdg_surface: message.object,
                            parent: Some(parent_surface),
                            rect,
                        },
                    )?;
                    self.objects.insert(
                        message.object,
                        Object::XdgSurface {
                            surface,
                            wm_base,
                            role_object: Some(RoleObject::Popup(id)),
                            assigned: Some(RoleKind::Popup),
                            configure,
                            // Carried rather than cleared, for `get_toplevel`'s
                            // reason: this write-back says nothing about state
                            // it does not touch.
                            pending_geometry,
                        },
                    );
                    Ok(())
                }
                _ => Err(format!(
                    "unsupported xdg_surface request {}",
                    message.opcode
                )),
            },
            Object::Positioner(mut positioner) => match message.opcode {
                0 => {
                    args.finish()?;
                    self.remove_object(message.object)
                }
                // set_size
                1 => {
                    let width = args.i32()?;
                    let height = args.i32()?;
                    args.finish()?;
                    if width <= 0 || height <= 0 {
                        return self.fail_protocol(
                            XDG_POSITIONER_ERROR_INVALID_INPUT,
                            &format!(
                                "xdg_positioner {} size {width}x{height} is not positive",
                                message.object
                            ),
                        );
                    }
                    positioner.set_size(width, height);
                    self.objects
                        .insert(message.object, Object::Positioner(positioner));
                    Ok(())
                }
                // set_anchor_rect. A zero-area rectangle is ACCEPTED and names
                // a point; only a negative side is a mistake, which is the one
                // the protocol gives an error for.
                2 => {
                    let x = args.i32()?;
                    let y = args.i32()?;
                    let width = args.i32()?;
                    let height = args.i32()?;
                    args.finish()?;
                    if width < 0 || height < 0 {
                        return self.fail_protocol(
                            XDG_POSITIONER_ERROR_INVALID_INPUT,
                            &format!(
                                "xdg_positioner {} anchor rectangle {width}x{height} is negative",
                                message.object
                            ),
                        );
                    }
                    positioner.set_anchor_rect(PositionerRect {
                        x,
                        y,
                        width,
                        height,
                    });
                    self.objects
                        .insert(message.object, Object::Positioner(positioner));
                    Ok(())
                }
                // set_anchor
                3 => {
                    let anchor = args.u32()?;
                    args.finish()?;
                    let Some(anchor) = Anchor::from_wire(anchor) else {
                        return self.fail_protocol(
                            XDG_POSITIONER_ERROR_INVALID_INPUT,
                            &format!(
                                "xdg_positioner {} anchor {anchor} is not one of the nine",
                                message.object
                            ),
                        );
                    };
                    positioner.set_anchor(anchor);
                    self.objects
                        .insert(message.object, Object::Positioner(positioner));
                    Ok(())
                }
                // set_gravity
                4 => {
                    let gravity = args.u32()?;
                    args.finish()?;
                    let Some(gravity) = Gravity::from_wire(gravity) else {
                        return self.fail_protocol(
                            XDG_POSITIONER_ERROR_INVALID_INPUT,
                            &format!(
                                "xdg_positioner {} gravity {gravity} is not one of the nine",
                                message.object
                            ),
                        );
                    };
                    positioner.set_gravity(gravity);
                    self.objects
                        .insert(message.object, Object::Positioner(positioner));
                    Ok(())
                }
                // set_constraint_adjustment. Recorded and not yet acted on:
                // every bit is permission for td to move a popup that does not
                // fit, and td does not move one yet.
                5 => {
                    let adjustment = args.u32()?;
                    args.finish()?;
                    positioner.set_constraint_adjustment(adjustment);
                    self.objects
                        .insert(message.object, Object::Positioner(positioner));
                    Ok(())
                }
                // set_offset
                6 => {
                    let x = args.i32()?;
                    let y = args.i32()?;
                    args.finish()?;
                    positioner.set_offset((x, y));
                    self.objects
                        .insert(message.object, Object::Positioner(positioner));
                    Ok(())
                }
                _ => Err(format!(
                    "unsupported xdg_positioner request {}",
                    message.opcode
                )),
            },
            Object::XdgPopup { xdg_surface, .. } => match message.opcode {
                0 => {
                    args.finish()?;
                    let Some(Object::XdgSurface {
                        surface, wm_base, ..
                    }) = self.objects.get(&xdg_surface).cloned()
                    else {
                        return Err(format!(
                            "xdg_popup {} lost xdg_surface {xdg_surface}",
                            message.object
                        ));
                    };
                    // The protocol's `not_the_topmost_popup`, which for a TREE
                    // is a popup with a live child: destroying it would leave a
                    // submenu hanging off a menu that has gone.
                    let child = self.objects.iter().find_map(|(id, object)| match object {
                        Object::XdgPopup { parent, .. } if *parent == Some(surface) => Some(*id),
                        _ => None,
                    });
                    if let Some(child) = child {
                        return self.fail_protocol_on(
                            wm_base,
                            XDG_WM_BASE_ERROR_NOT_THE_TOPMOST_POPUP,
                            &format!(
                                "xdg_popup {} is destroyed before xdg_popup {child}, which hangs off it",
                                message.object
                            ),
                        );
                    }
                    self.remove_object(message.object)?;
                    self.unmap_popup(surface)?;
                    let Some(Object::XdgSurface {
                        role_object,
                        configure,
                        ..
                    }) = self.objects.get_mut(&xdg_surface)
                    else {
                        return Err(format!(
                            "xdg_popup {} lost xdg_surface {xdg_surface}",
                            message.object
                        ));
                    };
                    // The xdg_surface outlives its role object and may be given
                    // another, exactly as a destroyed toplevel leaves one — and
                    // the tracker is replaced for that reason rather than
                    // tidiness. One left initialised says the surface has
                    // already had its first configure, so the NEXT role would
                    // never be sent one and a client waiting on it hangs with
                    // no window.
                    *role_object = None;
                    *configure = Arc::new(Mutex::new(ConfigureTracker::new()));
                    Ok(())
                }
                // grab. Accepted and not yet acted on: td dismisses no popup
                // of its own VOLITION — it signals the dismissals its own
                // take-downs cause, but nothing here closes a menu because a
                // click landed outside it — so a grab it recorded would be a
                // promise about keyboard and pointer routing that nothing
                // keeps.
                1 => {
                    args.u32()?;
                    args.u32()?;
                    args.finish()
                }
                _ => Err(format!("unsupported xdg_popup request {}", message.opcode)),
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
                    let Some(Object::XdgSurface {
                        surface,
                        wm_base,
                        assigned,
                        pending_geometry,
                        ..
                    }) = self.objects.get(&xdg_surface).cloned()
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
                            wm_base,
                            role_object: None,
                            // The ROLE outlives the role object: a second
                            // toplevel here is a client reusing the window it
                            // already measured, and a popup would be a
                            // different role on a surface that already has one.
                            assigned,
                            configure: Arc::new(Mutex::new(ConfigureTracker::new())),
                            // Carried where the tracker is replaced: a geometry
                            // is the xdg_surface's for as long as the object
                            // lives, and this one is still here.
                            pending_geometry,
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

    /// Everything the compositor has sent and nothing more. `receive_messages`
    /// needs a count, which a test that only cares whether ONE particular
    /// event arrived would have to derive from every other event the sequence
    /// happens to produce.
    fn drain_messages(stream: &mut UnixStream) -> Vec<wire::Message> {
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut messages = Vec::new();
        let mut scratch = [0u8; 4096];
        while let Ok(received) = stream.read(&mut scratch) {
            if received == 0 {
                break;
            }
            bytes.extend_from_slice(scratch.get(..received).unwrap());
            while let Some(message) = wire::take(&mut bytes).unwrap() {
                messages.push(message);
            }
        }
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        // Leftover bytes are a message split across the timeout. Nothing here
        // writes one in pieces, and a drain that dropped it would be a test
        // failing for a message that WAS sent.
        assert!(bytes.is_empty(), "the drain timed out mid-message");
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
                    wm_base: 50,
                    role_object: Some(RoleObject::Toplevel(10)),
                    assigned: Some(RoleKind::Toplevel),
                    configure: Arc::new(Mutex::new(ConfigureTracker::new())),
                    pending_geometry: None,
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
    /// state the decoration and window-geometry tests both start from, which is
    /// why it is named for the window rather than for either interface. The peer
    /// is returned so a test can read what the compositor sent, and the
    /// framebuffer path so it can be cleaned up.
    fn toplevel_fixture(stem: &str) -> (Client, UnixStream, Arc<Mutex<Runtime>>, PathBuf) {
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
        // A REAL shell object, and deliberately not 12: the decoration tests
        // give that id a decoration, so a fixture claiming it as the surface's
        // xdg_wm_base would have the error tests agreeing with a number that
        // names the wrong interface. Two fixtures with two shell objects is
        // also what makes "the error names the surface's OWN one" testable.
        client.insert(50, Object::XdgWmBase).unwrap();
        client
            .insert(
                9,
                Object::XdgSurface {
                    surface: 5,
                    wm_base: 50,
                    role_object: Some(RoleObject::Toplevel(10)),
                    assigned: Some(RoleKind::Toplevel),
                    configure: Arc::new(Mutex::new(ConfigureTracker::new())),
                    pending_geometry: None,
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
            toplevel_fixture("decoration-configure");

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
            toplevel_fixture("decoration-set-mode");

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
            toplevel_fixture("decoration-same-mode");

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
            toplevel_fixture("decoration-bad-mode");

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
            toplevel_fixture("decoration-twice");

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
            toplevel_fixture("decoration-reask");

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
        let (mut client, _peer, runtime, framebuffer_path) = toplevel_fixture("decoration-mapped");
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
            toplevel_fixture("decoration-attached");
        let pool_path = framebuffer_path.with_extension("pool");
        fs::write(&pool_path, [1u8, 2, 3, 0]).unwrap();
        client.objects.insert(
            5,
            Object::Surface(SurfaceState {
                pending_buffer: Some(PendingBuffer::Buffer {
                    offset: (0, 0),
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
            toplevel_fixture("decoration-orphan");

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
            toplevel_fixture("decoration-order");

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
            toplevel_fixture("decoration-error-object");

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
            toplevel_fixture("decoration-orphan-object");

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
            toplevel_fixture("decoration-reconfigure");
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
            toplevel_fixture("decoration-late-create");
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
            toplevel_fixture("decoration-manager-destroy");

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
            toplevel_fixture("decoration-non-toplevel");

        let error = get_decoration(&mut client, 12, 5).unwrap_err();
        assert!(error.contains("non-toplevel 5"), "{error}");
        assert!(!client.objects.contains_key(&12));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// `set_window_geometry(x, y, width, height)` on the xdg_surface at 9.
    fn set_geometry(
        client: &mut Client,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), String> {
        let mut request_body = wire::Builder::new();
        request_body.i32(x);
        request_body.i32(y);
        request_body.i32(width);
        request_body.i32(height);
        client.dispatch(request(9, 3, request_body).unwrap(), &mut VecDeque::new())
    }

    /// `wl_surface.commit` on the surface at 5, which is what applies the
    /// double-buffered state above.
    fn commit(client: &mut Client) -> Result<(), String> {
        client.dispatch(
            request(5, 6, wire::Builder::new()).unwrap(),
            &mut VecDeque::new(),
        )
    }

    /// The fixture's surface, told which xdg_surface it is under. The fixture
    /// leaves the role unset, and a commit skips the whole XDG path without it.
    fn adopt_role(client: &mut Client) {
        client.objects.insert(
            5,
            Object::Surface(SurfaceState {
                role: Some(SurfaceRole::Xdg(9)),
                ..SurfaceState::default()
            }),
        );
    }

    fn surface_key() -> SurfaceKey {
        SurfaceKey {
            client: 88,
            object: 5,
        }
    }

    /// The geometry is double-buffered state: the request records it and the
    /// wl_surface's own commit is what applies it. A compositor that applied it
    /// on arrival would crop a window to bounds measured for a buffer that has
    /// not been attached yet.
    #[test]
    fn a_window_geometry_waits_for_the_commit_that_applies_it() {
        let (mut client, _peer, runtime, framebuffer_path) = toplevel_fixture("geometry-pending");
        adopt_role(&mut client);

        set_geometry(&mut client, 12, 13, 40, 30).unwrap();
        assert_eq!(runtime.lock().unwrap().window_geometry(surface_key()), None);

        commit(&mut client).unwrap();
        assert_eq!(
            runtime.lock().unwrap().window_geometry(surface_key()),
            Some(WindowGeometry {
                x: 12,
                y: 13,
                width: 40,
                height: 30,
            })
        );

        // The pending slot is spent, so a commit that says nothing about the
        // geometry leaves the one already in force rather than re-applying it.
        commit(&mut client).unwrap();
        assert!(matches!(
            client.objects.get(&9),
            Some(Object::XdgSurface {
                pending_geometry: None,
                ..
            })
        ));
        assert_eq!(
            runtime.lock().unwrap().window_geometry(surface_key()),
            Some(WindowGeometry {
                x: 12,
                y: 13,
                width: 40,
                height: 30,
            })
        );

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// `invalid_size`, by the code the protocol names for it. Every side is
    /// tried in both wrong directions: zero and negative are one refusal in the
    /// code and two different mistakes in a toolkit.
    #[test]
    fn a_window_geometry_with_a_side_that_is_not_positive_is_refused() {
        let (mut client, _peer, runtime, framebuffer_path) = toplevel_fixture("geometry-invalid");
        adopt_role(&mut client);

        for (width, height) in [(0, 30), (40, 0), (-1, 30), (40, -1), (0, 0)] {
            let error = set_geometry(&mut client, 0, 0, width, height).unwrap_err();
            assert!(error.contains("is not positive"), "{error}");
            assert_eq!(
                client.protocol_error_code, XDG_SURFACE_ERROR_INVALID_SIZE,
                "{width}x{height} was refused under the wrong code"
            );
            // Refused before anything was recorded, so a client whose error is
            // somehow survived is not holding a geometry the compositor also
            // thinks is pending.
            assert!(matches!(
                client.objects.get(&9),
                Some(Object::XdgSurface {
                    pending_geometry: None,
                    ..
                })
            ));
        }
        commit(&mut client).unwrap();
        assert_eq!(runtime.lock().unwrap().window_geometry(surface_key()), None);

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// A commit that takes the buffer AWAY still applies the geometry it
    /// carries. That is the one shape where the crop is applied on its own, and
    /// it matters for the same reason the unmap keeps the previous one: nothing
    /// re-sends a geometry, so one dropped here is a window tiled by its shadow
    /// margins from its next map onwards.
    #[test]
    fn an_unmapping_commit_still_applies_its_geometry() {
        let (mut client, _peer, runtime, framebuffer_path) = toplevel_fixture("geometry-unmap");
        adopt_role(&mut client);
        // wl_surface.attach with a null buffer, which is the unmap.
        let mut attach = wire::Builder::new();
        attach.u32(0);
        attach.i32(0);
        attach.i32(0);
        client
            .dispatch(request(5, 1, attach).unwrap(), &mut VecDeque::new())
            .unwrap();

        set_geometry(&mut client, 7, 8, 21, 22).unwrap();
        commit(&mut client).unwrap();
        assert_eq!(
            runtime.lock().unwrap().window_geometry(surface_key()),
            Some(WindowGeometry {
                x: 7,
                y: 8,
                width: 21,
                height: 22,
            })
        );

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// "A role must be assigned before any other requests are made to the
    /// xdg_surface object", so a geometry set before `get_toplevel` is
    /// `not_constructed` rather than state held for a role nobody has asked
    /// for. It is the FIRST thing checked, before the arguments.
    #[test]
    fn a_geometry_before_the_role_object_is_refused() {
        let (mut client, _peer, runtime, framebuffer_path) = toplevel_fixture("geometry-early");
        // Back to an xdg_surface with no role object, which is what a client
        // has if it sets a geometry this early.
        client.objects.insert(
            9,
            Object::XdgSurface {
                surface: 5,
                wm_base: 50,
                role_object: None,
                assigned: None,
                configure: Arc::new(Mutex::new(ConfigureTracker::new())),
                pending_geometry: None,
            },
        );
        client.objects.remove(&10);

        let error = set_geometry(&mut client, 6, 6, 20, 20).unwrap_err();
        assert!(error.contains("before its role object"), "{error}");
        assert_eq!(
            client.protocol_error_code,
            XDG_SURFACE_ERROR_NOT_CONSTRUCTED
        );
        // A geometry that is ALSO the wrong size answers for the role first: a
        // client that has not asked for a window yet is told that rather than
        // told about its arguments.
        let error = set_geometry(&mut client, 0, 0, 0, 20).unwrap_err();
        assert!(error.contains("before its role object"), "{error}");
        assert_eq!(
            client.protocol_error_code,
            XDG_SURFACE_ERROR_NOT_CONSTRUCTED
        );
        assert!(matches!(
            client.objects.get(&9),
            Some(Object::XdgSurface {
                pending_geometry: None,
                ..
            })
        ));

        // Nothing to apply once the role does arrive, since nothing was kept.
        let mut request_body = wire::Builder::new();
        request_body.u32(10);
        client
            .dispatch(request(9, 1, request_body).unwrap(), &mut VecDeque::new())
            .unwrap();
        adopt_role(&mut client);
        commit(&mut client).unwrap();
        assert_eq!(runtime.lock().unwrap().window_geometry(surface_key()), None);

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// The geometry is the xdg_surface's, so the toplevel going does not take
    /// it and the xdg_surface going does. The distinction is reachable: a
    /// client that destroys its toplevel and asks for another on the same
    /// xdg_surface is reusing the window it already measured.
    #[test]
    fn a_geometry_outlives_the_toplevel_and_dies_with_the_xdg_surface() {
        let (mut client, mut peer, runtime, framebuffer_path) =
            toplevel_fixture("geometry-destroy");
        adopt_role(&mut client);
        set_geometry(&mut client, 4, 4, 24, 24).unwrap();
        commit(&mut client).unwrap();
        assert!(runtime
            .lock()
            .unwrap()
            .window_geometry(surface_key())
            .is_some());

        // A second geometry that no commit has applied, so the destroy below is
        // asked about both halves of this state at once.
        set_geometry(&mut client, 5, 5, 25, 25).unwrap();
        client
            .dispatch(
                request(10, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(
            runtime
                .lock()
                .unwrap()
                .window_geometry(surface_key())
                .is_some(),
            "the toplevel took the xdg_surface's geometry with it"
        );
        assert!(
            matches!(
                client.objects.get(&9),
                Some(Object::XdgSurface {
                    pending_geometry: Some(WindowGeometry {
                        x: 5,
                        y: 5,
                        width: 25,
                        height: 25,
                    }),
                    ..
                })
            ),
            "the toplevel took a geometry the client is still owed"
        );

        client
            .dispatch(
                request(9, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(runtime.lock().unwrap().window_geometry(surface_key()), None);

        // Drained rather than asserted about: the destroys above answer with
        // delete_id, which this test is not the one that checks.
        let mut scratch = [0u8; 256];
        let _ = peer.read(&mut scratch);
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
                    wm_base: 50,
                    role_object: Some(RoleObject::Toplevel(10)),
                    assigned: Some(RoleKind::Toplevel),
                    configure: Arc::clone(&tracker),
                    pending_geometry: None,
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
                    wm_base: 50,
                    role_object: Some(RoleObject::Toplevel(10)),
                    assigned: Some(RoleKind::Toplevel),
                    configure: Arc::new(Mutex::new(configure)),
                    pending_geometry: None,
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
                role_object: None,
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
                role_object: Some(RoleObject::Toplevel(11)),
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
        // A surface whose xdg_surface has been destroyed is refused too: what
        // a wl_surface takes once is the ROLE, and the object that carried it
        // going away does not hand the surface back.
        client
            .insert(
                13,
                Object::Surface(SurfaceState {
                    role: Some(SurfaceRole::XdgRetired),
                    ..SurfaceState::default()
                }),
            )
            .unwrap();
        let mut retired = wire::Builder::new();
        retired.u32(serial);
        retired.u32(13);
        retired.i32(0);
        retired.i32(0);
        // Cleared first: the refusal above left the same code behind, and an
        // `Err` does not reset it, so the assertion would hold for any failure.
        client.protocol_error_code = 0;
        let error = client
            .dispatch(request(6, 0, retired).unwrap(), &mut VecDeque::new())
            .unwrap_err();
        assert!(
            error.contains("already has an incompatible role"),
            "{error}"
        );
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
            pending_buffer: Some(PendingBuffer::Buffer {
                object: 7,
                buffer,
                offset: (0, 0),
            }),
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
    /// for not appearing. Its OFFSET is not refused with it: the pixels are
    /// td's to decline, where the offset is the client's account of where its
    /// contents now are, and a hotspot left behind would put the next frame
    /// — one td does accept — beside where the client asked for it.
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
        set_cursor.i32(5);
        set_cursor.i32(9);
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
            pending_buffer: Some(PendingBuffer::Buffer {
                object: 7,
                buffer,
                offset: (3, 4),
            }),
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

        // The refused commit's offset took, which only a frame td ACCEPTS can
        // show: this one carries none of its own, so the hotspot it lands at
        // is the refused attach's and nothing else.
        let small = Buffer {
            serial: 2,
            file: Arc::new(File::open(&pool_path).unwrap()),
            offset: 0,
            width: 1,
            height: 1,
            stride: 4,
            format: SHM_XRGB8888,
        };
        client.insert(8, Object::Buffer(small.clone())).unwrap();
        let accepted = SurfaceState {
            pending_buffer: Some(PendingBuffer::Buffer {
                object: 8,
                buffer: small,
                offset: (0, 0),
            }),
            role: Some(SurfaceRole::Cursor),
            ..SurfaceState::default()
        };
        client.objects.insert(5, Object::Surface(accepted.clone()));
        client.commit_surface(5, accepted).unwrap();
        assert_eq!(
            runtime.lock().unwrap().cursor_image(),
            Some((2, 5, 1, 1)),
            "a refused cursor frame dropped the offset it arrived with"
        );

        stop.stop();
        runtime.lock().unwrap().unsubscribe_keyboard(2);
        fs::remove_file(framebuffer_path).unwrap();
        fs::remove_file(pool_path).unwrap();
    }

    /// The two `i32`s an attach carries are the surface offset at the version
    /// td advertises — `wl_surface.offset` replaces them only from version 5 —
    /// and `wl_pointer.set_cursor` decrements the hotspot by them. Driven over
    /// the WIRE because the reading is the half that can go wrong: the
    /// arguments used to be parsed and dropped, and a pair read in the other
    /// order is a cursor offset diagonally from where the client put it.
    #[test]
    fn an_attach_offset_reaches_the_cursor_it_was_sent_for() {
        let stem = format!(
            "td-wayland-cursor-offset-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let pool_path = std::env::temp_dir().join(format!("{stem}.pool"));
        fs::write(&pool_path, [7u8; 2 * 2 * 4]).unwrap();
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
        let serial = wire::Cursor::new(&enter.first().unwrap().payload)
            .u32()
            .unwrap();

        // Deliberately unequal, and unequal to each other's offsets: 5 - 2 and
        // 9 - 3 differ, so a pair read the other way round would be caught.
        let mut set_cursor = wire::Builder::new();
        set_cursor.u32(serial);
        set_cursor.u32(5);
        set_cursor.i32(5);
        set_cursor.i32(9);
        client
            .dispatch(request(6, 0, set_cursor).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .insert(
                7,
                Object::Buffer(Buffer {
                    serial: 1,
                    file: Arc::new(File::open(&pool_path).unwrap()),
                    offset: 0,
                    width: 2,
                    height: 2,
                    stride: 8,
                    format: SHM_XRGB8888,
                }),
            )
            .unwrap();

        let mut attach = wire::Builder::new();
        attach.u32(7);
        attach.i32(2);
        attach.i32(3);
        client
            .dispatch(request(5, 1, attach).unwrap(), &mut VecDeque::new())
            .unwrap();
        // Double-buffered like the contents it rides with: the attach alone
        // moves nothing.
        assert_eq!(runtime.lock().unwrap().cursor_image(), None);

        client
            .dispatch(
                request(5, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(
            runtime.lock().unwrap().cursor_image(),
            Some((3, 6, 2, 2)),
            "the attach offset did not reach the hotspot"
        );

        // A NULL attach carries an offset like any other, and it is the one
        // that reaches nothing on screen when it is applied: the surface has
        // no pixels until the next frame arrives, and the move is there when
        // it does.
        let mut unmap = wire::Builder::new();
        unmap.u32(0);
        unmap.i32(1);
        unmap.i32(2);
        client
            .dispatch(request(5, 1, unmap).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(5, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(runtime.lock().unwrap().cursor_image(), None);

        let mut again = wire::Builder::new();
        again.u32(7);
        again.i32(0);
        again.i32(0);
        client
            .dispatch(request(5, 1, again).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(5, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(
            runtime.lock().unwrap().cursor_image(),
            Some((2, 4, 2, 2)),
            "a null attach's offset was dropped"
        );

        stop.stop();
        runtime.lock().unwrap().unsubscribe_keyboard(2);
        fs::remove_file(framebuffer_path).unwrap();
        fs::remove_file(pool_path).unwrap();
    }

    /// A TILE ignores the offset, which is the answer rather than an omission:
    /// td places the window and the geometry names which part of the buffer
    /// fills that place, so shifting the contents on top would move a window
    /// inside its own tile and leave a gap at the edge it moved from.
    #[test]
    fn a_tiles_attach_offset_moves_neither_its_pixels_nor_its_crop() {
        let stem = format!(
            "td-wayland-tile-offset-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let framebuffer_path = std::env::temp_dir().join(format!("{stem}.fb"));
        let pool_path = std::env::temp_dir().join(format!("{stem}.pool"));
        let pixels = [0x21u8, 0x43, 0x65, 0];
        fs::write(&pool_path, pixels).unwrap();
        let framebuffer = Framebuffer::test_file(
            &framebuffer_path,
            8,
            crate::scene::least_output_height(2),
            32,
        )
        .unwrap();
        let (server, _peer) = UnixStream::pair().unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        let mut client = Client::new(2, server, Arc::clone(&runtime), test_keymap()).unwrap();
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
                    wm_base: 50,
                    role_object: Some(RoleObject::Toplevel(10)),
                    assigned: Some(RoleKind::Toplevel),
                    configure: Arc::new(Mutex::new(configure)),
                    pending_geometry: None,
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

        let mut commit = |x: i32, y: i32| {
            let mut attach = wire::Builder::new();
            attach.u32(7);
            attach.i32(x);
            attach.i32(y);
            client
                .dispatch(request(5, 1, attach).unwrap(), &mut VecDeque::new())
                .unwrap();
            client
                .dispatch(
                    request(5, 6, wire::Builder::new()).unwrap(),
                    &mut VecDeque::new(),
                )
                .unwrap();
            fs::read(&framebuffer_path).unwrap()
        };
        let square = commit(0, 0);
        assert!(square.as_chunks::<4>().0.contains(&pixels));
        // The same pixels attached at an offset paint the same screen. Read off
        // the framebuffer rather than off the layout, because a tile's rect is
        // the layout's to choose either way — what an offset could have moved
        // is the ink inside it.
        assert_eq!(commit(7, 9), square);
        // Nor is it a crop: the surface still has no geometry of its own, which
        // is the other rectangle an offset could have been mistaken for.
        assert_eq!(
            runtime.lock().unwrap().window_geometry(SurfaceKey {
                client: 2,
                object: 5,
            }),
            None
        );

        fs::remove_file(framebuffer_path).unwrap();
        fs::remove_file(pool_path).unwrap();
    }

    /// `xdg_wm_base` at 12, a positioner at 30, and a second wl_surface (6)
    /// with its own xdg_surface (13) for the popup to be. The fixture's own
    /// xdg_surface 9 is the parent, and it already has a toplevel.
    fn popup_fixture(stem: &str) -> (Client, UnixStream, Arc<Mutex<Runtime>>, PathBuf) {
        let (mut client, peer, runtime, framebuffer_path) = toplevel_fixture(stem);
        client.insert(12, Object::XdgWmBase).unwrap();
        client
            .insert(6, Object::Surface(SurfaceState::default()))
            .unwrap();
        // Through the WIRE rather than inserted, so the wl_surface's own role
        // is set the way a client sets it — a surface with no role skips the
        // whole XDG path at commit, and the popup would never be configured.
        let mut xdg_surface = wire::Builder::new();
        xdg_surface.u32(13);
        xdg_surface.u32(6);
        client
            .dispatch(request(12, 2, xdg_surface).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut create = wire::Builder::new();
        create.u32(30);
        client
            .dispatch(request(12, 1, create).unwrap(), &mut VecDeque::new())
            .unwrap();
        (client, peer, runtime, framebuffer_path)
    }

    /// The two rules a positioner is INCOMPLETE without, plus an anchor and a
    /// gravity: a menu anchored to the bottom-left of a 4x6 item and dropping
    /// down-right from it.
    fn menu_rules(client: &mut Client) {
        let mut size = wire::Builder::new();
        size.i32(40);
        size.i32(20);
        client
            .dispatch(request(30, 1, size).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut anchor_rect = wire::Builder::new();
        for value in [10, 20, 4, 6] {
            anchor_rect.i32(value);
        }
        client
            .dispatch(request(30, 2, anchor_rect).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut anchor = wire::Builder::new();
        anchor.u32(6);
        client
            .dispatch(request(30, 3, anchor).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut gravity = wire::Builder::new();
        gravity.u32(8);
        client
            .dispatch(request(30, 4, gravity).unwrap(), &mut VecDeque::new())
            .unwrap();
    }

    /// `get_popup(id, parent, positioner)` on the xdg_surface at 13.
    fn get_popup(client: &mut Client, id: u32, parent: u32, positioner: u32) -> Result<(), String> {
        let mut request_body = wire::Builder::new();
        request_body.u32(id);
        request_body.u32(parent);
        request_body.u32(positioner);
        client.dispatch(request(13, 2, request_body).unwrap(), &mut VecDeque::new())
    }

    /// A client opening a menu is no longer disconnected: the positioner's
    /// rules place it, and the placement is what the client is told in
    /// `xdg_popup.configure` before the `xdg_surface.configure` that makes the
    /// pair one configuration.
    #[test]
    fn a_popup_is_placed_by_its_positioner_and_told_where() {
        let (mut client, mut peer, _runtime, framebuffer_path) = popup_fixture("popup-place");
        menu_rules(&mut client);
        get_popup(&mut client, 14, 9, 30).unwrap();

        // Double-buffered like every other xdg role: the request creates the
        // object and the wl_surface's own commit is what configures it.
        client
            .dispatch(
                request(6, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        let messages = receive_messages(&mut peer, 2);
        let placed = messages.first().unwrap();
        assert_eq!((placed.object, placed.opcode), (14, XDG_POPUP_CONFIGURE));
        let mut payload = wire::Cursor::new(&placed.payload);
        // Anchored bottom-left of (10, 20, 4x6) is (10, 26); a bottom-right
        // gravity hangs the surface off that point by nothing.
        assert_eq!(payload.i32().unwrap(), 10);
        assert_eq!(payload.i32().unwrap(), 26);
        assert_eq!(payload.i32().unwrap(), 40);
        assert_eq!(payload.i32().unwrap(), 20);
        payload.finish().unwrap();
        // The xdg_surface serial comes SECOND: it is what makes the placement
        // above one atomic configuration rather than a size on its own.
        let serial = messages.get(1).unwrap();
        assert_eq!((serial.object, serial.opcode), (13, 0));

        // The positioner may be destroyed straight after, because the rules
        // were COPIED: the popup keeps where it was put.
        client
            .dispatch(
                request(30, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(matches!(
            client.objects.get(&14),
            Some(Object::XdgPopup {
                rect,
                parent: Some(5),
                ..
            })
                if *rect == PositionerRect { x: 10, y: 26, width: 40, height: 20 }
        ));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// One popup, MAPPED: a real parent window, a menu placed by `menu_rules`,
    /// its configure acknowledged, and a one-pixel buffer committed. Both
    /// paths are the caller's to remove.
    fn mapped_popup(stem: &str) -> (Client, UnixStream, Arc<Mutex<Runtime>>, PathBuf, PathBuf) {
        let pool_name = format!(
            "td-wayland-{stem}-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let pool_path = std::env::temp_dir().join(format!("{pool_name}.pool"));
        fs::write(&pool_path, [0x21u8, 0x43, 0x65, 0]).unwrap();
        let (mut client, mut peer, runtime, framebuffer_path) = popup_fixture(stem);
        // The parent has to be a real window for the layout to hold anything.
        adopt_role(&mut client);
        commit(&mut client).unwrap();

        menu_rules(&mut client);
        get_popup(&mut client, 14, 9, 30).unwrap();
        client
            .dispatch(
                request(6, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        // Four: the PARENT's own initial configure pair from its commit above,
        // then the popup's. The popup's xdg_surface serial is the last of them.
        let configured = receive_messages(&mut peer, 4);
        let serial = wire::Cursor::new(&configured.get(3).unwrap().payload)
            .u32()
            .unwrap();
        let mut ack = wire::Builder::new();
        ack.u32(serial);
        client
            .dispatch(request(13, 4, ack).unwrap(), &mut VecDeque::new())
            .unwrap();

        client
            .insert(
                7,
                Object::Buffer(Buffer {
                    serial: 1,
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
            .dispatch(request(6, 1, attach).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(6, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        (client, peer, runtime, framebuffer_path, pool_path)
    }

    /// The parent window's surface, and the popup's.
    const POPUP_PARENT_KEY: SurfaceKey = SurfaceKey {
        client: 88,
        object: 5,
    };
    const POPUP_KEY: SurfaceKey = SurfaceKey {
        client: 88,
        object: 6,
    };

    /// A popup's buffer floats over its parent rather than joining the
    /// arrangement. A menu that entered the layout would take half the screen
    /// off the window it belongs to.
    #[test]
    fn a_popups_buffer_floats_over_its_parent_rather_than_tiling() {
        let (mut client, _peer, runtime, framebuffer_path, pool_path) = mapped_popup("popup-float");
        let parent_key = POPUP_PARENT_KEY;
        let popup_key = POPUP_KEY;
        let snapshot = runtime.lock().unwrap().layout_snapshot();
        assert!(
            !snapshot.contains_key(&popup_key),
            "a popup joined the layout"
        );
        assert_eq!(
            runtime.lock().unwrap().popup_placement(popup_key),
            Some(PopupPlacement {
                parent: parent_key,
                x: 10,
                y: 26,
                width: 40,
                height: 20,
            })
        );

        // Destroying the popup takes it off the screen and hands the
        // xdg_surface back, which may be given another role.
        client
            .dispatch(
                request(14, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(runtime.lock().unwrap().popup_placement(popup_key), None);
        assert!(matches!(
            client.objects.get(&13),
            Some(Object::XdgSurface {
                role_object: None,
                ..
            })
        ));

        let _ = fs::remove_file(&framebuffer_path);
        let _ = fs::remove_file(&pool_path);
    }

    /// Destroying a popup gives back its BYTES and its CONFIGURE. Neither is
    /// visible on screen, and both are the kind of thing a menu-heavy
    /// application hits within a minute: the bytes because a client that opens
    /// and dismisses menus is disconnected for buffers td is not holding, and
    /// the tracker because one left initialised says the first configure has
    /// already been sent — so the next role on that xdg_surface never gets
    /// one, and a client waiting on it hangs with no window.
    #[test]
    fn a_destroyed_popup_gives_back_its_bytes_and_its_configure() {
        let (mut client, _peer, _runtime, framebuffer_path, pool_path) =
            mapped_popup("popup-bytes");
        assert_eq!(client.mapped_bytes.get(&6).copied(), Some(4));
        let held = client.mapped_total;
        client
            .dispatch(
                request(14, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(client.mapped_bytes.get(&6), None);
        assert_eq!(client.mapped_total, held.saturating_sub(4));
        let Some(Object::XdgSurface { configure, .. }) = client.objects.get(&13) else {
            panic!("the xdg_surface went with its role object");
        };
        let tracker = configure.lock().unwrap();
        assert!(!tracker.initial_sent());
        assert!(!tracker.can_attach());

        let _ = fs::remove_file(&framebuffer_path);
        let _ = fs::remove_file(&pool_path);
    }

    /// A NULL attach takes a popup down the other way — the client keeping the
    /// role object and dropping the pixels — and owes the same two things back
    /// as the destroy above, plus the placement.
    #[test]
    fn a_null_attach_takes_a_popup_down_and_unconfigures_it() {
        let (mut client, _peer, runtime, framebuffer_path, pool_path) =
            mapped_popup("popup-detach");
        let held = client.mapped_total;
        let mut attach = wire::Builder::new();
        attach.u32(0);
        attach.i32(0);
        attach.i32(0);
        client
            .dispatch(request(6, 1, attach).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(6, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(runtime.lock().unwrap().popup_placement(POPUP_KEY), None);
        assert_eq!(client.mapped_bytes.get(&6), None);
        assert_eq!(client.mapped_total, held.saturating_sub(4));
        let Some(Object::XdgSurface { configure, .. }) = client.objects.get(&13) else {
            panic!("the xdg_surface went with its buffer");
        };
        assert!(!configure.lock().unwrap().can_attach());

        let _ = fs::remove_file(&framebuffer_path);
        let _ = fs::remove_file(&pool_path);
    }

    /// A popup's window geometry reaches the scene. A menu's is what a toolkit
    /// draws its shadow OUTSIDE, so one dropped on the way would anchor the
    /// shadow's corner where the client asked the menu to be and clip the menu
    /// away at the far edge.
    #[test]
    fn a_popups_window_geometry_reaches_the_scene() {
        let (mut client, _peer, runtime, framebuffer_path, pool_path) =
            mapped_popup("popup-geometry");
        // No buffer behind this one: the commit arm that carries a popup's
        // state when nothing was attached.
        let mut geometry = wire::Builder::new();
        for value in [4, 4, 10, 10] {
            geometry.i32(value);
        }
        client
            .dispatch(request(13, 3, geometry).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(6, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(
            runtime.lock().unwrap().window_geometry(POPUP_KEY),
            Some(WindowGeometry {
                x: 4,
                y: 4,
                width: 10,
                height: 10,
            })
        );

        // And again on a commit that DOES attach, which is the ordinary shape:
        // a toolkit sends the geometry with the frame it belongs to.
        let mut geometry = wire::Builder::new();
        for value in [6, 6, 8, 8] {
            geometry.i32(value);
        }
        client
            .dispatch(request(13, 3, geometry).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut attach = wire::Builder::new();
        attach.u32(7);
        attach.i32(0);
        attach.i32(0);
        client
            .dispatch(request(6, 1, attach).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(6, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(
            runtime.lock().unwrap().window_geometry(POPUP_KEY),
            Some(WindowGeometry {
                x: 6,
                y: 6,
                width: 8,
                height: 8,
            })
        );

        let _ = fs::remove_file(&framebuffer_path);
        let _ = fs::remove_file(&pool_path);
    }

    /// An incomplete positioner is `invalid_positioner` rather than a guess: a
    /// popup placed by rules the client never finished would appear somewhere
    /// it never asked for.
    #[test]
    fn an_incomplete_positioner_is_refused_at_get_popup() {
        let (mut client, _peer, _runtime, framebuffer_path) = popup_fixture("popup-incomplete");
        let mut size = wire::Builder::new();
        size.i32(40);
        size.i32(20);
        client
            .dispatch(request(30, 1, size).unwrap(), &mut VecDeque::new())
            .unwrap();

        let error = get_popup(&mut client, 14, 9, 30).unwrap_err();
        assert!(error.contains("incomplete"), "{error}");
        assert_eq!(
            client.protocol_error_code,
            XDG_WM_BASE_ERROR_INVALID_POSITIONER
        );
        // Raised on the `xdg_wm_base`, which is whose error code this is. On
        // the xdg_surface the request arrived at, 5 is `invalid_size`.
        assert_eq!(client.protocol_error_object, Some(12));
        // Nothing was created for a popup that was refused.
        assert!(!client.objects.contains_key(&14));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// The parent must be an xdg_surface of this client that has a role object.
    /// A null parent is legal only so another protocol can supply one, and td
    /// implements no such protocol — so a popup that arrived that way could
    /// never be placed at all.
    #[test]
    fn a_popup_parent_that_could_never_be_mapped_is_refused() {
        let (mut client, _peer, _runtime, framebuffer_path) = popup_fixture("popup-parent");
        menu_rules(&mut client);

        let error = get_popup(&mut client, 14, 0, 30).unwrap_err();
        assert!(error.contains("no xdg_surface"), "{error}");
        assert_eq!(
            client.protocol_error_code,
            XDG_WM_BASE_ERROR_INVALID_POPUP_PARENT
        );
        // The shell object's code, so the shell object's error: 3 on the
        // xdg_surface would decode as `unconfigured_buffer`.
        assert_eq!(client.protocol_error_object, Some(12));

        // An xdg_surface with no role object of its own cannot have been
        // mapped, so it cannot be a parent either. Object 13 is the popup's own
        // xdg_surface, which has none.
        let error = get_popup(&mut client, 15, 13, 30).unwrap_err();
        assert!(error.contains("unconstructed"), "{error}");
        assert_eq!(
            client.protocol_error_code,
            XDG_WM_BASE_ERROR_INVALID_POPUP_PARENT
        );
        assert_eq!(client.protocol_error_object, Some(12));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// A ROLE is permanent where a role OBJECT is not. A client may destroy an
    /// xdg_toplevel and build another — reusing the window it already measured
    /// — but the surface that carried a menu may not come back as a window and
    /// be tiled, nor a window's as a menu.
    #[test]
    fn a_surfaces_role_outlives_the_role_object_that_carried_it() {
        let (mut client, _peer, _runtime, framebuffer_path) = popup_fixture("popup-role");
        menu_rules(&mut client);
        get_popup(&mut client, 14, 9, 30).unwrap();
        client
            .dispatch(
                request(14, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        // The xdg_surface is roleless again — and still a popup's.
        let mut toplevel = wire::Builder::new();
        toplevel.u32(15);
        let error = client
            .dispatch(request(13, 1, toplevel).unwrap(), &mut VecDeque::new())
            .unwrap_err();
        assert!(error.contains("xdg_popup"), "{error}");
        assert_eq!(client.protocol_error_code, XDG_WM_BASE_ERROR_ROLE);
        assert_eq!(client.protocol_error_object, Some(12));
        assert!(!client.objects.contains_key(&15));

        // A second popup on it IS allowed: the role is the same one.
        get_popup(&mut client, 16, 9, 30).unwrap();

        // Again from a surface made by a DIFFERENT shell object, because the
        // refusal has to name the one this surface came from rather than
        // whichever id the fixture happens to use everywhere.
        client.insert(51, Object::XdgWmBase).unwrap();
        client
            .insert(24, Object::Surface(SurfaceState::default()))
            .unwrap();
        let mut second = wire::Builder::new();
        second.u32(25);
        second.u32(24);
        client
            .dispatch(request(51, 2, second).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut popup = wire::Builder::new();
        popup.u32(26);
        popup.u32(9);
        popup.u32(30);
        client
            .dispatch(request(25, 2, popup).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(26, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        let mut toplevel = wire::Builder::new();
        toplevel.u32(27);
        let error = client
            .dispatch(request(25, 1, toplevel).unwrap(), &mut VecDeque::new())
            .unwrap_err();
        assert!(error.contains("xdg_popup"), "{error}");
        assert_eq!(client.protocol_error_code, XDG_WM_BASE_ERROR_ROLE);
        assert_eq!(client.protocol_error_object, Some(51));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// And the same rule the other way round, which is the direction that
    /// would put a former menu into the layout.
    #[test]
    fn a_windows_surface_may_not_come_back_as_a_menu() {
        let (mut client, _peer, _runtime, framebuffer_path) = popup_fixture("popup-role-back");
        // Object 10 is the toplevel on xdg_surface 9; destroying it leaves the
        // xdg_surface roleless, as a client reusing a window does.
        client
            .dispatch(
                request(10, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        menu_rules(&mut client);
        let mut popup = wire::Builder::new();
        popup.u32(14);
        popup.u32(13);
        popup.u32(30);
        let error = client
            .dispatch(request(9, 2, popup).unwrap(), &mut VecDeque::new())
            .unwrap_err();
        assert!(error.contains("xdg_toplevel"), "{error}");
        assert_eq!(client.protocol_error_code, XDG_WM_BASE_ERROR_ROLE);
        // xdg_surface 9's own shell object, which is NOT the one the popup
        // surfaces were made by: the id is the surface's rather than a
        // constant every error happens to agree with.
        assert_eq!(client.protocol_error_object, Some(50));
        assert!(!client.objects.contains_key(&14));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// The toplevel half of role permanence, driven through `get_toplevel`
    /// over the wire rather than from a fixture that hand-writes the record.
    /// Every other test starts from an xdg_surface whose `assigned` was
    /// inserted by the fixture, so the write in `get_toplevel` itself was
    /// pinned by nothing and a mutation of it survived the suite.
    #[test]
    fn a_toplevel_records_the_role_its_surface_may_come_back_as() {
        let (mut client, _peer, _runtime, framebuffer_path) = popup_fixture("popup-role-wire");
        client
            .insert(20, Object::Surface(SurfaceState::default()))
            .unwrap();
        let mut xdg_surface = wire::Builder::new();
        xdg_surface.u32(21);
        xdg_surface.u32(20);
        client
            .dispatch(request(12, 2, xdg_surface).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut toplevel = wire::Builder::new();
        toplevel.u32(22);
        client
            .dispatch(request(21, 1, toplevel).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(22, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();

        // Roleless again, and still a window's.
        menu_rules(&mut client);
        let mut popup = wire::Builder::new();
        popup.u32(23);
        popup.u32(13);
        popup.u32(30);
        let error = client
            .dispatch(request(21, 2, popup).unwrap(), &mut VecDeque::new())
            .unwrap_err();
        assert!(error.contains("xdg_toplevel"), "{error}");
        assert_eq!(client.protocol_error_code, XDG_WM_BASE_ERROR_ROLE);
        assert_eq!(client.protocol_error_object, Some(12));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// A role request on an xdg_surface that still HOLDS one is
    /// `already_constructed`, the xdg_surface's own error — this being the
    /// case a buggy toolkit actually reaches, where the permanent-role rule
    /// above is about a surface whose role object has gone.
    #[test]
    fn a_second_role_object_on_one_xdg_surface_is_already_constructed() {
        let (mut client, _peer, _runtime, framebuffer_path) = popup_fixture("popup-second-role");
        menu_rules(&mut client);
        get_popup(&mut client, 14, 9, 30).unwrap();

        let mut toplevel = wire::Builder::new();
        toplevel.u32(15);
        let error = client
            .dispatch(request(13, 1, toplevel).unwrap(), &mut VecDeque::new())
            .unwrap_err();
        assert!(error.contains("already has an xdg_popup"), "{error}");
        assert_eq!(
            client.protocol_error_code,
            XDG_SURFACE_ERROR_ALREADY_CONSTRUCTED
        );
        // The xdg_surface's OWN error, so raised on the xdg_surface: no object
        // override, which is what `None` here means.
        assert_eq!(client.protocol_error_object, None);
        assert!(!client.objects.contains_key(&15));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// Destroy the window a menu hangs off, in the order the protocol asks
    /// for: the role object, then the xdg_surface, then the wl_surface.
    fn destroy_the_parent_window(client: &mut Client) {
        for object in [10u32, 9, 5] {
            client
                .dispatch(
                    request(object, 0, wire::Builder::new()).unwrap(),
                    &mut VecDeque::new(),
                )
                .unwrap();
        }
    }

    /// A menu whose window is destroyed does not follow the id that window's
    /// surface leaves behind. An id is not an identity: Wayland recycles them
    /// and td retires them with `wl_display.delete_id` precisely so a client
    /// may, so an edge left holding the NUMBER comes to name whatever takes it
    /// next — a menu drawn on, and taking the clicks of, a window that never
    /// opened it.
    #[test]
    fn a_menu_whose_window_is_destroyed_does_not_follow_a_reissued_id() {
        let (mut client, mut peer, runtime, framebuffer_path, pool_path) =
            mapped_popup("popup-reissued");
        assert!(runtime.lock().unwrap().popup_placement(POPUP_KEY).is_some());

        // A SECOND window with a menu of its own, which the destroy below must
        // leave alone: the sweep is about one surface, and one that cleared
        // every edge would shut every menu in the client when any window
        // closed.
        client
            .insert(70, Object::Surface(SurfaceState::default()))
            .unwrap();
        let mut other = wire::Builder::new();
        other.u32(71);
        other.u32(70);
        client
            .dispatch(request(12, 2, other).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut other_toplevel = wire::Builder::new();
        other_toplevel.u32(72);
        client
            .dispatch(
                request(71, 1, other_toplevel).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        client
            .insert(73, Object::Surface(SurfaceState::default()))
            .unwrap();
        let mut other_xdg = wire::Builder::new();
        other_xdg.u32(74);
        other_xdg.u32(73);
        client
            .dispatch(request(12, 2, other_xdg).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut other_popup = wire::Builder::new();
        other_popup.u32(75);
        other_popup.u32(71);
        other_popup.u32(30);
        client
            .dispatch(request(74, 2, other_popup).unwrap(), &mut VecDeque::new())
            .unwrap();

        destroy_the_parent_window(&mut client);
        assert!(matches!(
            client.objects.get(&14),
            Some(Object::XdgPopup { parent: None, .. })
        ));
        assert!(matches!(
            client.objects.get(&75),
            Some(Object::XdgPopup {
                parent: Some(70),
                ..
            })
        ));
        // The scene dropped it when its parent surface went.
        assert_eq!(runtime.lock().unwrap().popup_placement(POPUP_KEY), None);

        // The id is back in the client's pool, and something else takes it.
        client
            .insert(5, Object::Surface(SurfaceState::default()))
            .unwrap();
        let mut reissued = wire::Builder::new();
        reissued.u32(60);
        reissued.u32(5);
        client
            .dispatch(request(12, 2, reissued).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut toplevel = wire::Builder::new();
        toplevel.u32(61);
        client
            .dispatch(request(60, 1, toplevel).unwrap(), &mut VecDeque::new())
            .unwrap();

        // The menu recommits its buffer, which is what a repaint is. It is
        // placed NOWHERE rather than onto the window that now holds id 5.
        drain_messages(&mut peer);
        let mut attach = wire::Builder::new();
        attach.u32(7);
        attach.i32(0);
        attach.i32(0);
        client
            .dispatch(request(6, 1, attach).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(6, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(runtime.lock().unwrap().popup_placement(POPUP_KEY), None);
        // NOWHERE means the layout too, and this is the half that assertion
        // cannot see: a surface that joined the arrangement is absent from the
        // popup map exactly as an unplaced one is, so a menu tiled as a window
        // would pass the line above.
        let snapshot = runtime.lock().unwrap().layout_snapshot();
        assert!(
            !snapshot.contains_key(&POPUP_KEY),
            "an orphaned menu joined the layout"
        );
        // The buffer comes BACK. td is not holding it, so a client left
        // waiting for the release would stall on its own next frame — and the
        // bytes stop being charged for the same reason.
        assert!(
            drain_messages(&mut peer)
                .iter()
                .any(|message| (message.object, message.opcode) == (7, 0)),
            "an orphaned menu kept the buffer it was given"
        );
        assert_eq!(client.mapped_bytes.get(&6), None);

        // Again, because once is not the interesting case: a client that has
        // not noticed its window is gone repaints on a timer, and an orphan
        // that could only be taken down once would fail on the second.
        let mut again = wire::Builder::new();
        again.u32(7);
        again.i32(0);
        again.i32(0);
        client
            .dispatch(request(6, 1, again).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(6, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(runtime.lock().unwrap().popup_placement(POPUP_KEY), None);

        // A geometry set while orphaned is not SPENT by these commits. They
        // happen and apply nothing, and `get_popup` carries a pending geometry
        // across a rebuilt role object — so spending it would leave a menu
        // rebuilt on this xdg_surface without the size its client last set,
        // with nothing having reported the loss.
        let mut measured = wire::Builder::new();
        for value in [1, 1, 4, 4] {
            measured.i32(value);
        }
        client
            .dispatch(request(13, 3, measured).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut once_more = wire::Builder::new();
        once_more.u32(7);
        once_more.i32(0);
        once_more.i32(0);
        client
            .dispatch(request(6, 1, once_more).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(6, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(
            matches!(
                client.objects.get(&13),
                Some(Object::XdgSurface {
                    pending_geometry: Some(_),
                    ..
                })
            ),
            "an orphan's commit spent a geometry it could not apply"
        );

        // A bufferless commit does not reach the tile path either: an orphan
        // is a POPUP with nowhere to go, not a surface that has stopped being
        // one, so its geometry is not stored as a window's would be.
        let mut geometry = wire::Builder::new();
        for value in [1, 1, 4, 4] {
            geometry.i32(value);
        }
        client
            .dispatch(request(13, 3, geometry).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(6, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(runtime.lock().unwrap().window_geometry(POPUP_KEY), None);

        let _ = fs::remove_file(&framebuffer_path);
        let _ = fs::remove_file(&pool_path);
    }

    /// The edge breaks with the SURFACE, not with the role objects above it.
    /// While the wl_surface lives its id still means what it meant, and a
    /// client may destroy an xdg_toplevel and build another on the same
    /// xdg_surface — so breaking the edge any earlier would orphan a menu
    /// whose window is about to come back.
    ///
    /// It also breaks EVERY edge naming that surface: two popups on one parent
    /// is ordinary — a menu and a tooltip — and one that stopped at the first
    /// would leave the other holding a number the client is free to reuse.
    #[test]
    fn a_menus_edge_breaks_with_its_parents_surface_and_no_sooner() {
        let (mut client, _peer, _runtime, framebuffer_path, pool_path) =
            mapped_popup("popup-edge-timing");

        // A second popup on the SAME parent surface.
        client
            .insert(80, Object::Surface(SurfaceState::default()))
            .unwrap();
        let mut second = wire::Builder::new();
        second.u32(81);
        second.u32(80);
        client
            .dispatch(request(12, 2, second).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut tooltip = wire::Builder::new();
        tooltip.u32(82);
        tooltip.u32(9);
        tooltip.u32(30);
        client
            .dispatch(request(81, 2, tooltip).unwrap(), &mut VecDeque::new())
            .unwrap();

        for above in [10u32, 9] {
            client
                .dispatch(
                    request(above, 0, wire::Builder::new()).unwrap(),
                    &mut VecDeque::new(),
                )
                .unwrap();
            assert!(
                matches!(
                    client.objects.get(&14),
                    Some(Object::XdgPopup {
                        parent: Some(5),
                        ..
                    })
                ),
                "the edge broke at object {above} rather than at the surface"
            );
        }

        client
            .dispatch(
                request(5, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        for popup in [14u32, 82] {
            assert!(
                matches!(
                    client.objects.get(&popup),
                    Some(Object::XdgPopup { parent: None, .. })
                ),
                "xdg_popup {popup} kept an edge to a destroyed surface"
            );
        }

        let _ = fs::remove_file(&framebuffer_path);
        let _ = fs::remove_file(&pool_path);
    }

    /// A cycle cannot be BUILT through the protocol, which is the claim
    /// DESIGN.md §3 argues and nothing at this level had checked — the scene's
    /// own cycle test hands the renderer one directly, which says the bound
    /// holds rather than that the server refuses to produce one.
    ///
    /// This is the shape that gets closest, and it turns on the sixth leg: a
    /// surface's placement leaves the scene when its popup role object is
    /// destroyed. Placements are keyed per surface and written only by that
    /// surface's own commit, so a placement that outlived its popup would sit
    /// there while a NEW popup on the other surface pointed back — two live
    /// edges, each made under all five object rules, closing a loop.
    #[test]
    fn a_recreated_popup_role_object_cannot_close_a_cycle() {
        let (mut client, mut peer, runtime, framebuffer_path, pool_path) =
            mapped_popup("popup-cycle");
        let submenu_key = SurfaceKey {
            client: 88,
            object: 20,
        };

        // A submenu hanging off the menu: surface 20, its xdg_surface, and a
        // popup parented on the menu's xdg_surface. Mapped, so the scene holds
        // an edge 20 -> 6.
        client
            .insert(20, Object::Surface(SurfaceState::default()))
            .unwrap();
        let mut shell = wire::Builder::new();
        shell.u32(21);
        shell.u32(20);
        client
            .dispatch(request(12, 2, shell).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut hanging = wire::Builder::new();
        hanging.u32(22);
        hanging.u32(13);
        hanging.u32(30);
        client
            .dispatch(request(21, 2, hanging).unwrap(), &mut VecDeque::new())
            .unwrap();
        map_popup_surface(&mut client, &mut peer, 20, 21, 27, &pool_path);
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .popup_placement(submenu_key)
                .map(|placement| placement.parent),
            Some(POPUP_KEY),
            "the submenu is not hanging off the menu"
        );

        // The submenu's popup object goes. Its PLACEMENT must go with it —
        // this is the leg.
        client
            .dispatch(
                request(22, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert_eq!(
            runtime.lock().unwrap().popup_placement(submenu_key),
            None,
            "a destroyed popup left its placement in the scene"
        );

        // Surface 20 takes a popup role object again, parented on the WINDOW
        // this time, and is left uncommitted: what it is for is to be a legal
        // parent below, since a parent with no role object is refused.
        let mut rebuilt = wire::Builder::new();
        rebuilt.u32(23);
        rebuilt.u32(9);
        rebuilt.u32(30);
        client
            .dispatch(request(21, 2, rebuilt).unwrap(), &mut VecDeque::new())
            .unwrap();

        // Now the menu's own popup goes — allowed, since the only popup that
        // named its surface was the one destroyed above — and the menu takes a
        // new one pointing the other way, at the submenu's surface.
        client
            .dispatch(
                request(14, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        let mut reversed = wire::Builder::new();
        reversed.u32(24);
        reversed.u32(21);
        reversed.u32(30);
        client
            .dispatch(request(13, 2, reversed).unwrap(), &mut VecDeque::new())
            .unwrap();
        map_popup_surface(&mut client, &mut peer, 6, 13, 28, &pool_path);

        // The edge really did reverse — so the refusal is not what stopped
        // this — and the other one is gone, so there is no loop.
        assert_eq!(
            runtime
                .lock()
                .unwrap()
                .popup_placement(POPUP_KEY)
                .map(|placement| placement.parent),
            Some(submenu_key),
            "the menu did not re-point at the submenu's surface"
        );
        assert_eq!(
            runtime.lock().unwrap().popup_placement(submenu_key),
            None,
            "both edges are live at once, which is a cycle"
        );

        let _ = fs::remove_file(&framebuffer_path);
        let _ = fs::remove_file(&pool_path);
    }

    /// The fixture's menu given a submenu and a sub-submenu, each mapped with
    /// a buffer of its own. THREE levels below the window, because a cascade
    /// that walked only one would still take a two-level tree down whole and
    /// so could not be told from one that walks them all.
    fn menu_tree(client: &mut Client, peer: &mut UnixStream, pool_path: &PathBuf) {
        for (surface, xdg_surface, popup, parent, buffer) in
            [(20u32, 21u32, 22u32, 13u32, 27u32), (40, 41, 42, 21, 47)]
        {
            client
                .insert(surface, Object::Surface(SurfaceState::default()))
                .unwrap();
            let mut shell = wire::Builder::new();
            shell.u32(xdg_surface);
            shell.u32(surface);
            client
                .dispatch(request(12, 2, shell).unwrap(), &mut VecDeque::new())
                .unwrap();
            let mut hanging = wire::Builder::new();
            hanging.u32(popup);
            hanging.u32(parent);
            hanging.u32(30);
            client
                .dispatch(
                    request(xdg_surface, 2, hanging).unwrap(),
                    &mut VecDeque::new(),
                )
                .unwrap();
            map_popup_surface(client, peer, surface, xdg_surface, buffer, pool_path);
        }
        assert_eq!(
            client.mapped_bytes.keys().copied().collect::<Vec<u32>>(),
            vec![6, 20, 40],
            "the menu tree is not charged the way this test needs it"
        );
    }

    /// A menu's pixels are charged to the client that sent them, and a window
    /// going down takes its whole menu tree with it. The scene discards those
    /// pixels; this ledger has to hear about it, or a client that opens and
    /// closes menus is charged for every one it ever opened and is eventually
    /// disconnected for buffers td threw away.
    ///
    /// The refund follows the CASCADE rather than a walk of this ledger's own.
    /// What went down is whatever the scene's walk reached, and a walk made
    /// here afterwards would read the edges that walk has already removed.
    #[test]
    fn a_window_hiding_refunds_the_menus_that_went_with_it() {
        let (mut client, mut peer, _runtime, framebuffer_path, pool_path) =
            mapped_popup("popup-refund-hide");
        menu_tree(&mut client, &mut peer, &pool_path);

        // A bufferless surface still takes this path, and what is under test
        // is the menus the window drags down rather than its own pixels.
        let mut detach = wire::Builder::new();
        detach.u32(0);
        detach.i32(0);
        detach.i32(0);
        client
            .dispatch(request(5, 1, detach).unwrap(), &mut VecDeque::new())
            .unwrap();
        commit(&mut client).unwrap();
        assert!(
            client.mapped_bytes.is_empty(),
            "a menu that went down with its window is still charged: {:?}",
            client.mapped_bytes
        );
        assert_eq!(client.mapped_total, 0);

        let _ = fs::remove_file(&framebuffer_path);
        let _ = fs::remove_file(&pool_path);
    }

    /// The same refund down the other take-down path. A menu closing takes its
    /// own submenus rather than a window taking everything, and the two reach
    /// the scene by different calls — so a refund on one of them is not a
    /// refund on the other.
    #[test]
    fn a_menu_closing_refunds_the_submenus_that_went_with_it() {
        let (mut client, mut peer, _runtime, framebuffer_path, pool_path) =
            mapped_popup("popup-refund-close");
        menu_tree(&mut client, &mut peer, &pool_path);

        let mut detach = wire::Builder::new();
        detach.u32(0);
        detach.i32(0);
        detach.i32(0);
        client
            .dispatch(request(6, 1, detach).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(6, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(
            client.mapped_bytes.is_empty(),
            "a submenu that went down with its menu is still charged: {:?}",
            client.mapped_bytes
        );
        assert_eq!(client.mapped_total, 0);

        let _ = fs::remove_file(&framebuffer_path);
        let _ = fs::remove_file(&pool_path);
    }

    /// The third take-down path, which looked unreachable and is not. A menu
    /// does not have to be CREATED after its window's role object goes — it
    /// only has to be repainted. Its popup object, its xdg_surface and its
    /// configure tracker all outlive the toplevel, so a second buffer attaches
    /// with no fresh acknowledgement and the placement goes back into the
    /// scene naming a parent that is no longer a window. The destroy then
    /// reaches `remove_surface` with a live tree under it.
    ///
    /// `get_popup` refusing a parent with no role object is what made this
    /// look impossible, and all it rules out is a NEW menu.
    #[test]
    fn a_menu_repainted_after_its_window_went_is_refunded_by_the_destroy() {
        let (mut client, _peer, _runtime, framebuffer_path, pool_path) =
            mapped_popup("popup-recommit");
        assert_eq!(
            client.mapped_bytes.get(&6).copied(),
            Some(4),
            "the menu is not charged to begin with"
        );

        // The toplevel goes. This unmaps the window and cascades the menu
        // away, refunding it down the `unmap_surface` path.
        client
            .dispatch(
                request(10, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(
            client.mapped_bytes.is_empty(),
            "the toplevel's own take-down left the menu charged: {:?}",
            client.mapped_bytes
        );

        // The menu's own objects are all still alive, so the client may
        // simply REPAINT it — no new popup is created, which is the step the
        // unreachability argument assumed was needed.
        client
            .insert(
                27,
                Object::Buffer(Buffer {
                    serial: 27,
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
        attach.u32(27);
        attach.i32(0);
        attach.i32(0);
        client
            .dispatch(request(6, 1, attach).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(6, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        // The step the whole test turns on. Without this charge the assertion
        // below would pass on a menu that was never re-entered in the scene,
        // and the test would stop covering `remove_surface` with nothing to
        // say so.
        assert_eq!(
            client.mapped_bytes.get(&6).copied(),
            Some(4),
            "the repaint did not put the menu back"
        );

        // The xdg_surface may go now: its role object was destroyed above.
        client
            .dispatch(
                request(9, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();

        // And the wl_surface, whose role is retired rather than Xdg(_).
        client
            .dispatch(
                request(5, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(
            client.mapped_bytes.is_empty(),
            "remove_surface reached a live popup tree and did not refund it: {:?}",
            client.mapped_bytes
        );
        assert_eq!(client.mapped_total, 0);

        let _ = fs::remove_file(&framebuffer_path);
        let _ = fs::remove_file(&pool_path);
    }

    /// Every popup td took down of its own accord, in the order it said so.
    ///
    /// Which objects are popups is asked of the CLIENT rather than listed
    /// here: a roster written down goes stale the moment a fixture grows a
    /// menu, and it goes stale silently — the missing event simply never
    /// appears and the assertion still passes.
    fn dismissals(client: &Client, peer: &mut UnixStream) -> Vec<u32> {
        drain_messages(peer)
            .iter()
            .filter(|message| {
                message.opcode == XDG_POPUP_POPUP_DONE
                    && matches!(
                        client.objects.get(&message.object),
                        Some(Object::XdgPopup { .. })
                    )
            })
            .map(|message| message.object)
            .collect()
    }

    /// A menu td stops drawing is still open as far as its client knows, until
    /// it is told. `popup_done` is the telling, and the protocol asks for it in
    /// the same order it makes a client destroy nested popups — topmost first
    /// — so a client that obeys by destroying each one as it hears never has
    /// to destroy a menu with a submenu still hanging off it, which is the
    /// `not_the_topmost_popup` error td already raises.
    #[test]
    fn a_window_going_tells_its_menus_they_were_dismissed_topmost_first() {
        let (mut client, mut peer, _runtime, framebuffer_path, pool_path) =
            mapped_popup("popup-dismiss-window");
        menu_tree(&mut client, &mut peer, &pool_path);
        assert!(
            dismissals(&client, &mut peer).is_empty(),
            "a menu was dismissed before anything took it down"
        );

        let mut detach = wire::Builder::new();
        detach.u32(0);
        detach.i32(0);
        detach.i32(0);
        client
            .dispatch(request(5, 1, detach).unwrap(), &mut VecDeque::new())
            .unwrap();
        commit(&mut client).unwrap();

        // 42 hangs off 22 hangs off 14, so this is the chain deepest first.
        // The cascade itself is breadth-first from the window, which is the
        // opposite order — a parent always reaches the list before its
        // submenus — so getting this right is a reversal and not an accident
        // of how the popups happen to be numbered.
        assert_eq!(
            dismissals(&client, &mut peer),
            vec![42, 22, 14],
            "the menu tree was not dismissed topmost first"
        );

        let _ = fs::remove_file(&framebuffer_path);
        let _ = fs::remove_file(&pool_path);
    }

    /// A client that closes its OWN menu is not told td dismissed it: the
    /// event means the compositor took the decision, and here the client did.
    /// Its submenus are the other case — nothing asked for those to go, so
    /// they are td's dismissal and hear about it.
    ///
    /// The menu is closed by a null attach rather than by destroying its
    /// `xdg_popup`, and that is forced rather than chosen: `xdg_popup.destroy`
    /// refuses with `not_the_topmost_popup` while a submenu hangs off it, so
    /// the destroy path can never reach a take-down with a cascade under it.
    #[test]
    fn closing_a_menu_dismisses_its_submenus_but_not_the_menu_itself() {
        let (mut client, mut peer, _runtime, framebuffer_path, pool_path) =
            mapped_popup("popup-dismiss-menu");
        menu_tree(&mut client, &mut peer, &pool_path);
        let _ = dismissals(&client, &mut peer);

        let mut detach = wire::Builder::new();
        detach.u32(0);
        detach.i32(0);
        detach.i32(0);
        client
            .dispatch(request(6, 1, detach).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(6, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();

        assert_eq!(
            dismissals(&client, &mut peer),
            vec![42, 22],
            "the client's own menu was dismissed back at it, or its submenus were not"
        );

        let _ = fs::remove_file(&framebuffer_path);
        let _ = fs::remove_file(&pool_path);
    }

    /// The one dismissal a cascade cannot reach. A popup still unmapped when
    /// its window goes is in no cascade — there is nothing of it on screen to
    /// take down — so it is never dismissed by one. Its client then commits
    /// the buffer it was preparing, and td decides against the menu right
    /// there, which is the only moment it can ever be told.
    ///
    /// Left untold, the client waits on a menu td silently discarded: it gets
    /// its buffer back and no configure, no pixels, and no reason.
    #[test]
    fn a_menu_orphaned_before_its_first_buffer_is_still_told() {
        let (mut client, mut peer, _runtime, framebuffer_path, pool_path) =
            mapped_popup("popup-dismiss-orphan");

        // A second menu on the same window, created and never mapped.
        client
            .insert(60, Object::Surface(SurfaceState::default()))
            .unwrap();
        let mut shell = wire::Builder::new();
        shell.u32(61);
        shell.u32(60);
        client
            .dispatch(request(12, 2, shell).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut hanging = wire::Builder::new();
        hanging.u32(62);
        hanging.u32(9);
        hanging.u32(30);
        client
            .dispatch(request(61, 2, hanging).unwrap(), &mut VecDeque::new())
            .unwrap();

        // As far through the map as a client gets before it has pixels: the
        // bufferless commit that asks for a configure, and the acknowledgement.
        // The BUFFER is what is withheld, and withholding it is what keeps this
        // popup out of the scene and so out of any cascade.
        client
            .dispatch(
                request(60, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        let serial = drain_messages(&mut peer)
            .iter()
            .rev()
            .find_map(|message| {
                ((message.object, message.opcode) == (61, 0))
                    .then(|| wire::Cursor::new(&message.payload).u32().unwrap())
            })
            .unwrap();
        let mut ack = wire::Builder::new();
        ack.u32(serial);
        client
            .dispatch(request(61, 4, ack).unwrap(), &mut VecDeque::new())
            .unwrap();

        // The window goes, all the way to its wl_surface, which is what breaks
        // the parent edges.
        for (object, opcode) in [(10u32, 0u16), (9, 0), (5, 0)] {
            client
                .dispatch(
                    request(object, opcode, wire::Builder::new()).unwrap(),
                    &mut VecDeque::new(),
                )
                .unwrap();
        }
        // The cascade dismissed the MAPPED menu and could not have reached
        // 62, which had no pixels to take down. Asserted rather than drained,
        // because it is the half this test's name turns on.
        assert_eq!(
            dismissals(&client, &mut peer),
            vec![14],
            "the cascade did not do what this test assumes it did"
        );

        // The client finishes what it started: a buffer for a menu that now
        // has nowhere to go.
        client
            .insert(
                63,
                Object::Buffer(Buffer {
                    serial: 63,
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
        attach.u32(63);
        attach.i32(0);
        attach.i32(0);
        client
            .dispatch(request(60, 1, attach).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(60, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();

        assert_eq!(
            dismissals(&client, &mut peer),
            vec![62],
            "a menu td decided against was not told it was dismissed"
        );

        let _ = fs::remove_file(&framebuffer_path);
        let _ = fs::remove_file(&pool_path);
    }

    /// The dismissal order follows the TREE, not the order the popups were
    /// placed in — and the two really can disagree, which is why this is a
    /// test rather than a remark.
    ///
    /// The protocol FORBIDS what this fixture does — "the parent of an
    /// xdg_popup must be mapped before the xdg_popup itself" — and td does not
    /// police it, which a compositor need not: `get_popup` checks only that
    /// the parent xdg_surface holds a role object. So a client that ignores
    /// the rule can commit a sub-submenu before the submenu it hangs off, and
    /// the scene's stacking order then runs the opposite way to the chain:
    /// here 40 is placed before its own parent 20 and carries the LOWER order.
    ///
    /// That the sequence is non-conformant is the point rather than a flaw in
    /// the fixture. Reversing the cascade is a property of the tree and holds
    /// whatever the client does; a stacking sort holds only while clients obey
    /// a rule nothing checks, so one misbehaving client could extract from it
    /// the very destroy order td refuses.
    ///
    /// Sorting the dismissal by that order — which reads as the obvious way to
    /// get "topmost first" — would put 20 ahead of 40, a parent ahead of its
    /// child, and a client obeying it would destroy a menu with a submenu
    /// still hanging off it. That is `not_the_topmost_popup`, which td itself
    /// raises. Reversing the cascade cannot make that mistake: the cascade is
    /// breadth-first, so a parent's index is always the lower one whatever
    /// order the placements happened in.
    #[test]
    fn a_submenu_placed_before_its_menu_is_still_dismissed_first() {
        let (mut client, mut peer, runtime, framebuffer_path, pool_path) =
            mapped_popup("popup-dismiss-order");

        // Both role objects first, so neither map below is blocked on the
        // other existing.
        for (surface, xdg_surface, popup, parent) in
            [(20u32, 21u32, 22u32, 13u32), (40, 41, 42, 21)]
        {
            client
                .insert(surface, Object::Surface(SurfaceState::default()))
                .unwrap();
            let mut shell = wire::Builder::new();
            shell.u32(xdg_surface);
            shell.u32(surface);
            client
                .dispatch(request(12, 2, shell).unwrap(), &mut VecDeque::new())
                .unwrap();
            let mut hanging = wire::Builder::new();
            hanging.u32(popup);
            hanging.u32(parent);
            hanging.u32(30);
            client
                .dispatch(
                    request(xdg_surface, 2, hanging).unwrap(),
                    &mut VecDeque::new(),
                )
                .unwrap();
        }

        // The DEEPER one first. This is the whole fixture: it gives 40 a lower
        // stacking order than the 20 it hangs off.
        map_popup_surface(&mut client, &mut peer, 40, 41, 27, &pool_path);
        map_popup_surface(&mut client, &mut peer, 20, 21, 47, &pool_path);
        let _ = dismissals(&client, &mut peer);

        // The inversion this test exists for, asserted rather than assumed:
        // bottom-first, 40 sits below the 20 it hangs off. Without it the
        // fixture would prove nothing, because a reversal and an order sort
        // agree whenever the placements happen to run with the chain.
        let key = |object| SurfaceKey { client: 88, object };
        assert_eq!(
            runtime.lock().unwrap().popup_stack(),
            vec![key(6), key(40), key(20)],
            "the submenu was not placed below its own menu"
        );

        let mut detach = wire::Builder::new();
        detach.u32(0);
        detach.i32(0);
        detach.i32(0);
        client
            .dispatch(request(5, 1, detach).unwrap(), &mut VecDeque::new())
            .unwrap();
        commit(&mut client).unwrap();

        assert_eq!(
            dismissals(&client, &mut peer),
            vec![42, 22, 14],
            "the dismissal followed the placements rather than the chain"
        );

        let _ = fs::remove_file(&framebuffer_path);
        let _ = fs::remove_file(&pool_path);
    }

    /// Take a popup surface through the whole map: the bufferless commit that
    /// asks for a configure, the acknowledgement, and the buffer.
    ///
    /// Wants a FRESH configure tracker — an already-configured surface is sent
    /// no initial pair, and the serial scan below then matches nothing. Both
    /// callers have one because `xdg_popup.destroy` replaces the tracker. The
    /// LAST configure is the one taken, which is unambiguous only because the
    /// drain empties the socket first.
    fn map_popup_surface(
        client: &mut Client,
        peer: &mut UnixStream,
        surface: u32,
        xdg_surface: u32,
        buffer: u32,
        pool_path: &PathBuf,
    ) {
        client
            .dispatch(
                request(surface, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        let serial = drain_messages(peer)
            .iter()
            .rev()
            .find_map(|message| {
                ((message.object, message.opcode) == (xdg_surface, 0))
                    .then(|| wire::Cursor::new(&message.payload).u32().unwrap())
            })
            .unwrap();
        let mut ack = wire::Builder::new();
        ack.u32(serial);
        client
            .dispatch(request(xdg_surface, 4, ack).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .insert(
                buffer,
                Object::Buffer(Buffer {
                    serial: u64::from(buffer),
                    file: Arc::new(File::open(pool_path).unwrap()),
                    offset: 0,
                    width: 1,
                    height: 1,
                    stride: 4,
                    format: SHM_XRGB8888,
                }),
            )
            .unwrap();
        let mut attach = wire::Builder::new();
        attach.u32(buffer);
        attach.i32(0);
        attach.i32(0);
        client
            .dispatch(request(surface, 1, attach).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(surface, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
    }

    /// The OTHER edge that named a raw id. A wl_surface points at its
    /// xdg_surface by number, and that number goes back to the client when the
    /// xdg_surface is destroyed — so a role still holding it would resolve, on
    /// the surface's next commit, to whatever the client made next. Not a
    /// refusal but a CROSS-WIRING: the old surface would take a stranger's
    /// role object and configure tracker, and where that stranger is a menu it
    /// would join the popup graph — the one place a parent edge can be made to
    /// point at something younger, which is how a cycle gets built.
    #[test]
    fn a_surface_does_not_commit_through_a_reissued_xdg_surface_id() {
        let (mut client, _peer, _runtime, framebuffer_path) = toplevel_fixture("role-reissued");

        // A surface of its own, given a real role through the real request so
        // the role edge is the one the server writes.
        client
            .insert(60, Object::Surface(SurfaceState::default()))
            .unwrap();
        let mut shell = wire::Builder::new();
        shell.u32(61);
        shell.u32(60);
        client
            .dispatch(request(50, 2, shell).unwrap(), &mut VecDeque::new())
            .unwrap();
        assert!(matches!(
            client.objects.get(&60),
            Some(Object::Surface(SurfaceState {
                role: Some(SurfaceRole::Xdg(61)),
                ..
            }))
        ));

        // The xdg_surface goes, and with it the id — but not the role.
        client
            .dispatch(
                request(61, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(matches!(
            client.objects.get(&60),
            Some(Object::Surface(SurfaceState {
                role: Some(SurfaceRole::XdgRetired),
                ..
            }))
        ));

        // Another surface takes that number for an xdg_surface of its own.
        client
            .insert(70, Object::Surface(SurfaceState::default()))
            .unwrap();
        let mut reissued = wire::Builder::new();
        reissued.u32(61);
        reissued.u32(70);
        client
            .dispatch(request(50, 2, reissued).unwrap(), &mut VecDeque::new())
            .unwrap();

        let error = client
            .dispatch(
                request(60, 6, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap_err();
        assert!(
            error.contains("after its xdg_surface was destroyed"),
            "{error}"
        );

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// The half a stale id got WRONG in the other direction. Destroying the
    /// wl_surface is refused while its role object lives, and that question
    /// used to be asked by looking the id up — so a client that tore its shell
    /// objects down in the right order and then reused the number was refused
    /// for a role object that was already gone. A retired role answers it
    /// without a lookup, and a surface holding one is still not a cursor:
    /// the role is what is permanent, not the object that carried it.
    #[test]
    fn a_surface_outlives_its_xdg_surface_without_inheriting_its_id() {
        let (mut client, _peer, _runtime, framebuffer_path) =
            toplevel_fixture("role-retired-destroy");

        client
            .insert(60, Object::Surface(SurfaceState::default()))
            .unwrap();
        let mut shell = wire::Builder::new();
        shell.u32(61);
        shell.u32(60);
        client
            .dispatch(request(50, 2, shell).unwrap(), &mut VecDeque::new())
            .unwrap();
        client
            .dispatch(
                request(61, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();

        // The number comes back as somebody else's xdg_surface.
        client
            .insert(70, Object::Surface(SurfaceState::default()))
            .unwrap();
        let mut reissued = wire::Builder::new();
        reissued.u32(61);
        reissued.u32(70);
        client
            .dispatch(request(50, 2, reissued).unwrap(), &mut VecDeque::new())
            .unwrap();

        // A second xdg_surface is still refused — the role outlives the object
        // — and the destroy is not.
        let mut again = wire::Builder::new();
        again.u32(62);
        again.u32(60);
        let error = client
            .dispatch(request(50, 2, again).unwrap(), &mut VecDeque::new())
            .unwrap_err();
        assert!(error.contains("already has a role"), "{error}");
        client
            .dispatch(
                request(60, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(!client.objects.contains_key(&60));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// The same stale edge seen from the other side: it must not make a NEW
    /// menu undestroyable. `not_the_topmost_popup` scans for a popup naming
    /// this one's wl_surface, so an orphan still holding a recycled number
    /// would answer for a submenu that does not exist — and the client is
    /// disconnected for destroying a menu with nothing hanging off it.
    #[test]
    fn a_stale_parent_id_does_not_make_a_new_menu_undestroyable() {
        let (mut client, _peer, _runtime, framebuffer_path, pool_path) =
            mapped_popup("popup-stale-scan");
        destroy_the_parent_window(&mut client);

        // A fresh surface takes the dead window's id, and a menu of its own.
        client
            .insert(5, Object::Surface(SurfaceState::default()))
            .unwrap();
        let mut reissued = wire::Builder::new();
        reissued.u32(60);
        reissued.u32(5);
        client
            .dispatch(request(12, 2, reissued).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut popup = wire::Builder::new();
        popup.u32(61);
        popup.u32(13);
        popup.u32(30);
        client
            .dispatch(request(60, 2, popup).unwrap(), &mut VecDeque::new())
            .unwrap();

        client
            .dispatch(
                request(61, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(!client.objects.contains_key(&61));

        let _ = fs::remove_file(&framebuffer_path);
        let _ = fs::remove_file(&pool_path);
    }

    /// The numbers the shell is spoken in, pinned by VALUE, exactly as
    /// `the_decoration_protocol_numbers_are_the_protocols` pins the
    /// decoration's. Every other test compares what went out against these
    /// same constants, so one that agrees with the code and not with the
    /// protocol is one they would all still pass — while the client on the
    /// other end, which knows the real numbers, decodes something else. That
    /// misdecoding is the whole subject of this commit, so leaving four fresh
    /// numbers unpinned would be the same bug by another route.
    #[test]
    fn the_shell_protocol_numbers_are_the_protocols() {
        assert_eq!(XDG_WM_BASE_ERROR_ROLE, 0);
        assert_eq!(XDG_WM_BASE_ERROR_DEFUNCT_SURFACES, 1);
        assert_eq!(XDG_WM_BASE_ERROR_NOT_THE_TOPMOST_POPUP, 2);
        assert_eq!(XDG_WM_BASE_ERROR_INVALID_POPUP_PARENT, 3);
        assert_eq!(XDG_WM_BASE_ERROR_INVALID_POSITIONER, 5);
        assert_eq!(XDG_SURFACE_ERROR_NOT_CONSTRUCTED, 1);
        assert_eq!(XDG_SURFACE_ERROR_ALREADY_CONSTRUCTED, 2);
        assert_eq!(XDG_SURFACE_ERROR_INVALID_SIZE, 5);
        assert_eq!(XDG_POSITIONER_ERROR_INVALID_INPUT, 0);
        assert_eq!(XDG_POPUP_CONFIGURE, 0);
    }

    /// The popup direction of `already_constructed`. Deleting the check is not
    /// cosmetic: with the role KIND already popup the permanence rule below it
    /// does not fire either, so a second `get_popup` would insert a second
    /// xdg_popup and overwrite the role object — leaving the first one live,
    /// orphaned, and named by nothing.
    #[test]
    fn a_second_popup_on_one_xdg_surface_is_already_constructed_too() {
        let (mut client, _peer, _runtime, framebuffer_path) = popup_fixture("popup-second-popup");
        menu_rules(&mut client);
        get_popup(&mut client, 14, 9, 30).unwrap();

        let error = get_popup(&mut client, 15, 9, 30).unwrap_err();
        assert!(error.contains("already has an xdg_popup"), "{error}");
        assert_eq!(
            client.protocol_error_code,
            XDG_SURFACE_ERROR_ALREADY_CONSTRUCTED
        );
        assert_eq!(client.protocol_error_object, None);
        assert!(!client.objects.contains_key(&15));
        // The first is untouched, and the xdg_surface still names IT.
        assert!(matches!(
            client.objects.get(&13),
            Some(Object::XdgSurface {
                role_object: Some(RoleObject::Popup(14)),
                ..
            })
        ));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// An `xdg_wm_base` outlives the xdg_surfaces it made. That is the
    /// protocol's `defunct_surfaces`, and it is what keeps the id each
    /// xdg_surface carries meaningful: destroyed early it would be recycled,
    /// and an error later raised against "the xdg_wm_base" would name whatever
    /// took the number — the exact misdecoding naming it exists to prevent.
    #[test]
    fn a_shell_object_may_not_be_destroyed_before_its_surfaces() {
        let (mut client, _peer, _runtime, framebuffer_path) = popup_fixture("popup-defunct");
        let error = client
            .dispatch(
                request(12, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap_err();
        assert!(error.contains("xdg_surface"), "{error}");
        assert_eq!(
            client.protocol_error_code,
            XDG_WM_BASE_ERROR_DEFUNCT_SURFACES
        );
        // No override, so the error lands on the object the request arrived
        // at — which for this one IS the xdg_wm_base. Posting it on the
        // xdg_surface instead is the bug the rest of this commit removes.
        assert_eq!(client.protocol_error_object, None);
        assert!(client.objects.contains_key(&12));

        // One that made nothing goes, so the refusal is about this shell
        // object's OWN surfaces rather than about any surface existing.
        client.insert(40, Object::XdgWmBase).unwrap();
        client
            .dispatch(
                request(40, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(!client.objects.contains_key(&40));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// A menu may not be destroyed while a submenu hangs off it — the
    /// protocol's `not_the_topmost_popup`, which for a tree is a popup with a
    /// live child. It does NOT make a cycle unbuildable; a popup's parent is a
    /// wl_surface id and ids are recycled, which DESIGN.md §3 carries and the
    /// renderer's depth bound is what contains.
    ///
    /// The protocol's own wording is "tried to map or destroy a non-topmost
    /// popup", and only the destroy half is here. A naive map check would be
    /// wrong rather than missing: a menu recommits its buffer whenever the
    /// item under the pointer changes, so a popup mapping while a submenu is
    /// open is the ordinary case and not an ordering fault.
    #[test]
    fn a_menu_may_not_be_destroyed_before_the_submenu_hanging_off_it() {
        let (mut client, _peer, _runtime, framebuffer_path) = popup_fixture("popup-topmost");
        menu_rules(&mut client);
        get_popup(&mut client, 14, 9, 30).unwrap();

        // A submenu, on a wl_surface and xdg_surface of its own, parented on
        // the menu's xdg_surface.
        client
            .insert(16, Object::Surface(SurfaceState::default()))
            .unwrap();
        let mut xdg_surface = wire::Builder::new();
        xdg_surface.u32(17);
        xdg_surface.u32(16);
        client
            .dispatch(request(12, 2, xdg_surface).unwrap(), &mut VecDeque::new())
            .unwrap();
        let mut submenu = wire::Builder::new();
        submenu.u32(18);
        submenu.u32(13);
        submenu.u32(30);
        client
            .dispatch(request(17, 2, submenu).unwrap(), &mut VecDeque::new())
            .unwrap();

        let error = client
            .dispatch(
                request(14, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap_err();
        assert!(error.contains("hangs off it"), "{error}");
        assert_eq!(
            client.protocol_error_code,
            XDG_WM_BASE_ERROR_NOT_THE_TOPMOST_POPUP
        );
        assert_eq!(client.protocol_error_object, Some(12));
        // Refused means REFUSED: the menu is still there, so a client that
        // ignored the error has not been left with a half-destroyed stack.
        assert!(client.objects.contains_key(&14));

        // Innermost first is the order the protocol asks for, and it works.
        client
            .dispatch(
                request(18, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        client
            .dispatch(
                request(14, 0, wire::Builder::new()).unwrap(),
                &mut VecDeque::new(),
            )
            .unwrap();
        assert!(!client.objects.contains_key(&14));

        let _ = fs::remove_file(&framebuffer_path);
    }

    /// Every rule a positioner refuses, each with the protocol's own
    /// `invalid_input` and each raised on the POSITIONER, which is the object
    /// the client got wrong.
    #[test]
    fn a_positioner_rule_outside_its_range_is_refused_as_invalid_input() {
        let refusals: [(u16, Vec<i32>, &str); 5] = [
            (1, vec![0, 20], "not positive"),
            (1, vec![40, -1], "not positive"),
            (2, vec![0, 0, -1, 6], "negative"),
            (3, vec![9], "not one of the nine"),
            (4, vec![9], "not one of the nine"),
        ];
        for (opcode, values, expected) in refusals {
            let (mut client, _peer, _runtime, framebuffer_path) = popup_fixture("popup-rule");
            let mut body = wire::Builder::new();
            for value in &values {
                body.i32(*value);
            }
            let error = client
                .dispatch(request(30, opcode, body).unwrap(), &mut VecDeque::new())
                .unwrap_err();
            assert!(error.contains(expected), "{opcode}: {error}");
            assert_eq!(
                client.protocol_error_code, XDG_POSITIONER_ERROR_INVALID_INPUT,
                "{opcode}"
            );
            let _ = fs::remove_file(&framebuffer_path);
        }
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
