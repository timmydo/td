//! One bounded, private-Wayland FileChooser dialog, driven over the shared
//! `td_ui::client` turn loop.
//!
//! The portal spawns this on a FileChooser D-Bus request. It connects the
//! private compositor socket under a single-lane, bounded deadline (the
//! socket may not exist yet), hands the connected stream to the shared
//! client, and runs the client's turn loop as an [`App`]: it binds the one
//! privileged `td_portal_manager_v1` global the client does not know, asks
//! the compositor for a dialog against its toplevel, renders the chooser
//! through `td_ui`, and reports what the caller needs over [`Notice`].
//!
//! Two hardening properties are the portal's, not the shared client's, and
//! are asserted here rather than in td-ui:
//!
//!   * the private registry must advertise EXACTLY the eleven interfaces and
//!     versions in [`EXPECTED_GLOBALS`] as an exact set, so a foreign or
//!     tampered compositor is refused before the manager is bound;
//!   * physical keys map to chooser actions by their raw evdev keycode
//!     ([`key_action`]), never by the compositor's keymap symbols, so no
//!     keymap the compositor sends can move Accept onto an unexpected key.

use crate::file_chooser::{self, Action, Chooser, FileFilter, Mode, Outcome};
use crate::keyboard::{MOD_ALT, MOD_CAPS, MOD_CONTROL, MOD_LOGO, MOD_SHIFT};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use td_ui::client::{
    run, App, Client, ClipboardEvent, Handled, KeyboardEvent, DISPLAY, SURFACE,
};
use td_ui::client::Tag;
use td_ui::wire::{Builder, Cursor, Message};

type Result<T> = std::result::Result<T, String>;

/// The private portal registry the compositor must advertise: ten desktop
/// interfaces plus the single privileged `td_portal_manager_v1`, at these exact
/// versions. Listed in the compositor's announcement order for readability;
/// [`Dialog::bind_manager`] compares it as a SET, since the compositor's
/// registry-name assignment (and so `Client::globals()`'s order) is not the
/// announcement order. The shared client binds only the standard globals it
/// knows; the exactness of the whole set is the portal's to assert.
const EXPECTED_GLOBALS: [(&str, u32); 11] = [
    ("wl_compositor", 4),
    ("wl_subcompositor", 1),
    ("wl_shm", 1),
    ("wl_output", 4),
    ("xdg_wm_base", 1),
    ("zxdg_decoration_manager_v1", 1),
    ("wl_data_device_manager", 3),
    ("zxdg_exporter_v2", 1),
    ("zxdg_importer_v2", 1),
    ("wl_seat", 7),
    ("td_portal_manager_v1", 1),
];

/// The manager reports the dialog was dismissed with this state; any higher
/// value is out of range.
const PORTAL_DIALOG_DISMISSED: u32 = 2;
/// The private connect and its retries share this bound; the socket may not
/// exist yet when the worker starts.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_ATTEMPTS: usize = 200;
/// One keepalive `wl_display.sync` goes out this often so the compositor's
/// 30-second private-peer inactivity timeout never bounds user think-time.
const PULSE_INTERVAL_MS: u64 = 10_000;
/// Reaching interactivity, and later the dismiss acknowledgement, are each
/// bounded to this so a stalled peer cannot hang the worker.
const PROGRESS_DEADLINE_MS: u64 = 20_000;

/// What the worker hands back to the portal service thread. Preserved across
/// the td-ui migration so the service's `consume_dialog_notice` is unchanged:
/// the caller receives a cancellation handle, one first-frame report, and the
/// final outcome.
#[derive(Debug)]
pub enum Notice {
    Connected(UnixStream),
    Presented {
        width: usize,
        height: usize,
        checksum: u64,
    },
    Completed(Result<Outcome>),
}

/// The inputs the portal service supplies for one dialog.
#[derive(Debug)]
pub struct DialogConfig {
    pub socket: PathBuf,
    pub runtime_directory: PathBuf,
    pub title: String,
    pub parent_handle: String,
    pub app_id: String,
    pub host_root: PathBuf,
    pub guest_root: PathBuf,
    pub mode: Mode,
    pub accept_label: Option<String>,
    pub filter: Option<FileFilter>,
    pub connector: Arc<AtomicBool>,
}

/// The dialog's own objects in the shared client's table: the privileged
/// portal manager it binds (never destroyed — the connection ends first, so a
/// `delete_id` for it is the protocol error the client reports), and the
/// transient keepalive `wl_display.sync` callbacks. A `Pulse` callback is in
/// flight; a `PulseRetired` one has had its `done` and waits for the server's
/// `delete_id` to reclaim its slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Object {
    Manager,
    Pulse,
    PulseRetired,
}

impl Tag for Object {
    fn retired(self) -> bool {
        matches!(self, Object::PulseRetired)
    }
}

/// One live dialog over the shared client. `notice` is borrowed from the
/// worker so the same closure reports the first frame during the loop and the
/// outcome after it returns.
struct Dialog<'a, F: Fn(Notice) -> Result<()>> {
    client: Client<Object>,
    chooser: Chooser,
    window_title: String,
    parent_handle: String,
    notice: &'a F,
    /// The bound manager's object id, once [`Handled::Bound`] validated the
    /// registry and bound it.
    manager: Option<u32>,
    /// The current dialog extent, the toolkit default until the compositor
    /// configures one.
    size: (usize, usize),
    dirty: bool,
    /// The manager's last reported dialog state: `Some(0 | 1)` admits the
    /// first-frame notice, `Some(2)` ends the dismiss handshake.
    portal_state: Option<u32>,
    /// The last presented frame's callback has fired.
    frame_done: bool,
    /// The last presented frame's extent and checksum, reported to the caller
    /// once the dialog is interactive.
    presented: Option<(usize, usize, u64)>,
    presented_once: bool,
    /// A dismiss request is outstanding; the outcome waits for the manager's
    /// dismissed state before the loop ends.
    dismissing: bool,
    /// A keepalive sync callback in flight, awaiting its `done`; at most one.
    pulse: Option<u32>,
    /// When the last keepalive pulse went out (ms since the loop began).
    last_pulse: u64,
    /// When the post-connect handshake began (ms since the loop began), armed
    /// on the first turn; the progress deadline is measured from it.
    handshake_start: Option<u64>,
    /// When a dismiss request went out (ms since the loop began); the manager
    /// acknowledgement gets its own deadline from it.
    dismiss_start: Option<u64>,
    outcome: Option<Outcome>,
}

impl<'a, F: Fn(Notice) -> Result<()>> Dialog<'a, F> {
    fn new(config: DialogConfig, stream: UnixStream, notice: &'a F) -> Result<Self> {
        let window_title = if config.title.is_empty() {
            format!("{} — Open file", config.app_id)
        } else {
            format!("{} — {}", config.app_id, config.title)
        };
        let chooser = Chooser::open_with_options(
            &window_title,
            &config.host_root,
            &config.guest_root,
            config.mode,
            config.accept_label,
            config.filter,
        )?;
        // Pool files live in the portal's runtime directory, where the private
        // compositor can read them; the shared client creates and unlinks them
        // there, as the hand-rolled transport did.
        let client = Client::new(stream, config.runtime_directory)?;
        Ok(Self {
            client,
            chooser,
            window_title,
            parent_handle: config.parent_handle,
            notice,
            manager: None,
            size: (file_chooser::WIDTH, file_chooser::HEIGHT),
            dirty: true,
            portal_state: None,
            frame_done: false,
            presented: None,
            presented_once: false,
            dismissing: false,
            pulse: None,
            last_pulse: 0,
            handshake_start: None,
            dismiss_start: None,
            outcome: None,
        })
    }

    /// The initial roundtrip finished. Assert the private registry is exactly
    /// what td's compositor advertises, then bind the one privileged global
    /// the client does not know, create the dialog against the toplevel, and
    /// commit the initial surface state.
    fn bind_manager(&mut self) -> Result<()> {
        // SECURITY: the registry must advertise EXACTLY these eleven globals at
        // these versions, so no foreign or tampered compositor with a different
        // global surface is bound. `find_global` would accept a superset or a
        // higher version; only the whole-set check refuses that. The shared
        // client has recorded every advertised global by now (the sync callback
        // that raised `Bound` follows them all).
        //
        // Compare as a SET, not in advertisement order: the compositor assigns
        // fixed, non-monotonic registry names (td-compositor's GLOBAL_*
        // constants) and `Client::globals()` yields globals in name order, so
        // the advertised order is neither the announcement order nor a property
        // to pin. Sorted (interface, version) lists still reject any missing,
        // extra, duplicated, or version-shifted interface — a duplicate
        // interface cannot stand in for an omitted one.
        let mut advertised: Vec<(&str, u32)> = self.client.globals().collect();
        advertised.sort_unstable();
        let mut expected: Vec<(&str, u32)> = EXPECTED_GLOBALS.to_vec();
        expected.sort_unstable();
        if advertised != expected {
            return Err(format!(
                "private portal registry advertised {advertised:?}, expected {expected:?}"
            ));
        }
        let manager = self.client.allocate(Object::Manager)?;
        self.client.bind("td_portal_manager_v1", 1, manager)?;
        self.manager = Some(manager);
        self.client.set_title(&self.window_title)?;
        // get_dialog(surface, parent_handle, flags): register this toplevel as
        // the caller's modal file dialog. State comes back as an event on the
        // manager, not a new object.
        let mut request = Builder::new();
        request.u32(SURFACE);
        request.string(&self.parent_handle)?;
        request.u32(0);
        self.client.send(manager, 0, request, None)?;
        self.client.commit()
    }

    /// Apply a toplevel configure to the chooser and acknowledge it. A zero
    /// axis keeps the current extent; a negative one is protocol-illegal, so
    /// it is treated the same rather than cast to a huge extent.
    fn configure(&mut self, size: Option<(i32, i32)>, serial: u32) -> Result<()> {
        if let Some((width, height)) = size {
            let (current_w, current_h) = self.size;
            let width = if width <= 0 { current_w } else { width as usize };
            let height = if height <= 0 { current_h } else { height as usize };
            if (width, height) != self.size {
                self.chooser.set_viewport(width, height)?;
                self.size = (width, height);
                self.dirty = true;
            }
        }
        self.client.acknowledge(serial)
    }

    /// A press translated under the trusted map. The action is chosen by the
    /// raw evdev keycode, independent of the compositor's keymap symbols, so
    /// the physical Accept/Cancel positions cannot be remapped.
    fn key(&mut self, key: u32) -> Result<()> {
        let modifiers = self.client.input().modifiers;
        let mask = modifiers.depressed | modifiers.latched | modifiers.locked;
        if let Some(action) = key_action(key, mask, modifiers.group) {
            let outcome = self.chooser.apply(action)?;
            if outcome != Outcome::Pending {
                return self.begin_dismiss(outcome);
            }
            self.dirty = true;
        }
        Ok(())
    }

    /// A manager event: the dialog state `[surface, state]`. A dismissal the
    /// portal did not request is refused; the requested one ends the loop.
    fn portal_state(&mut self, message: &Message) -> Result<()> {
        let mut args = Cursor::new(&message.payload);
        let surface = args.u32()?;
        let state = args.u32()?;
        args.finish()?;
        if surface != SURFACE || state > PORTAL_DIALOG_DISMISSED {
            return Err(format!(
                "private portal manager answered surface {surface} with state {state}"
            ));
        }
        if state == PORTAL_DIALOG_DISMISSED && !self.dismissing {
            return Err("private portal dialog was dismissed without a request".into());
        }
        self.portal_state = Some(state);
        if state == PORTAL_DIALOG_DISMISSED {
            // The handshake is complete; leave the turn loop so the worker
            // reports the outcome it recorded.
            self.client.close();
        }
        self.report_presented()
    }

    /// Begin dismissing with `outcome`. The first outcome wins; a later one
    /// (a close arriving after a keypress accepted) is ignored. The loop stays
    /// until the manager confirms the dismissal.
    fn begin_dismiss(&mut self, outcome: Outcome) -> Result<()> {
        if self.dismissing {
            return Ok(());
        }
        self.dismissing = true;
        self.outcome = Some(outcome);
        let manager = self
            .manager
            .ok_or("private dialog dismissed before its manager was bound")?;
        let mut request = Builder::new();
        request.u32(SURFACE);
        self.client.send(manager, 1, request, None)
    }

    /// Present one frame when there is something to show and a buffer is free.
    /// The commit clears the shared client's startup deadline; the caller's
    /// first-frame notice is withheld separately, in [`Self::report_presented`].
    fn draw(&mut self) -> Result<()> {
        if !self.dirty || !self.client.can_present() {
            return Ok(());
        }
        let (width, height) = self.size;
        let mut checksum = 0u64;
        let Dialog {
            client, chooser, ..
        } = self;
        let presented = client.present(width, height, &mut |pixels| {
            let frame = chooser.render_sized(width, height)?;
            if frame.len() != pixels.len() {
                return Err(format!(
                    "chooser rendered {} bytes for a {}-byte {width}x{height} surface",
                    frame.len(),
                    pixels.len()
                ));
            }
            pixels.copy_from_slice(&frame);
            checksum = pixel_checksum(&frame);
            Ok(())
        })?;
        if presented {
            self.presented = Some((width, height, checksum));
            self.dirty = false;
            self.frame_done = false;
        }
        Ok(())
    }

    /// Report the first presented frame to the caller once the dialog is truly
    /// interactive: the compositor admitted it (`portal_state` 0 or 1), the
    /// keyboard is focused on it, its frame callback fired, and it released the
    /// frame's buffer (so the pixels are the compositor's, not still in
    /// flight). Only this signal waits; the buffer is committed as soon as it
    /// is drawn.
    fn report_presented(&mut self) -> Result<()> {
        if self.presented_once
            || !self.frame_done
            || self.client.buffers().iter().any(|buffer| buffer.busy())
            || !self.client.input().focused
            || !matches!(self.portal_state, Some(0 | 1))
        {
            return Ok(());
        }
        let Some((width, height, checksum)) = self.presented else {
            return Ok(());
        };
        self.presented_once = true;
        (self.notice)(Notice::Presented {
            width,
            height,
            checksum,
        })
    }

    /// Between turns: settle the first-frame notice, bound progress, and keep
    /// the private connection alive.
    fn maintain(&mut self, now: u64) -> Result<()> {
        // Re-check interactivity every turn. A buffer release is handled inside
        // the shared client with no event of its own, and can be the last of
        // the four gates to clear, so no event-driven call to report_presented
        // would fire; without this the caller never learns the dialog is up and
        // the handshake deadline below eventually kills it.
        self.report_presented()?;
        let start = *self.handshake_start.get_or_insert(now);
        // A stalled peer must not hang the worker. Until the dialog is
        // interactive the handshake is bounded; once a dismiss is requested its
        // acknowledgement is bounded afresh; an interactive dialog with no
        // dismiss outstanding is held open only by the pulse below.
        let deadline = if self.dismissing {
            Some((*self.dismiss_start.get_or_insert(now)).saturating_add(PROGRESS_DEADLINE_MS))
        } else if !self.presented_once {
            Some(start.saturating_add(PROGRESS_DEADLINE_MS))
        } else {
            None
        };
        if let Some(deadline) = deadline {
            if now >= deadline {
                return Err("private portal dialog stalled past its 20-second deadline".into());
            }
        }
        // The compositor closes a silent private client after 30 seconds; one
        // wl_display.sync every ten keeps it open so user think-time is not
        // bounded. But our own pulse resets the compositor's receive timer, so
        // a peer that answers nothing yet holds the socket would never be
        // detected: if the previous pulse is still unanswered a full interval
        // later, the peer has stalled and the worker fails closed.
        if now.saturating_sub(self.last_pulse) >= PULSE_INTERVAL_MS {
            if self.pulse.is_some() {
                return Err("private compositor did not answer the keepalive pulse".into());
            }
            let callback = self.client.allocate(Object::Pulse)?;
            self.client.words(DISPLAY, 0, &[callback])?;
            self.pulse = Some(callback);
            self.last_pulse = now;
        }
        Ok(())
    }

    fn dispatch(&mut self, message: Message) -> Result<()> {
        match self.client.handle(&message, 0)? {
            Handled::Done => Ok(()),
            Handled::Bound => self.bind_manager(),
            Handled::Configure { size, serial } => self.configure(size, serial),
            Handled::CloseRequested => self.begin_dismiss(Outcome::Cancelled),
            Handled::FrameDone => {
                self.frame_done = true;
                self.report_presented()
            }
            Handled::Keyboard(KeyboardEvent::Key { key, .. }) => self.key(key),
            // Focus arriving can open the first-frame gate; a compiled keymap
            // that failed is fatal, matching the old byte-exact rejection.
            Handled::Keyboard(KeyboardEvent::Focus(_) | KeyboardEvent::Ready) => {
                self.report_presented()
            }
            Handled::Keyboard(KeyboardEvent::Keymap(result)) => result,
            Handled::Keyboard(KeyboardEvent::Refused(_) | KeyboardEvent::Held(_)) => Ok(()),
            // The private registry is fixed for the dialog's life: any global
            // in the validated set leaving is anomalous and closes it.
            Handled::GlobalRemoved { .. } => {
                Err("private Wayland global was withdrawn mid-dialog".into())
            }
            Handled::SeatRemoved => Err("private seat was withdrawn mid-dialog".into()),
            Handled::Clipboard(ClipboardEvent::Released) => {
                Err("private data device manager was withdrawn mid-dialog".into())
            }
            // The portal reads neither the pointer nor the clipboard.
            Handled::Capabilities { .. } | Handled::Pointer(_) | Handled::Clipboard(_) => Ok(()),
            Handled::Unhandled => {
                if self.pulse == Some(message.object) && message.opcode == 0 {
                    // The keepalive round-trip closed; retire the callback so
                    // its slot is reclaimed on `delete_id`, and let the next
                    // pulse go out.
                    self.client.set_tag(message.object, Object::PulseRetired)?;
                    self.pulse = None;
                    Ok(())
                } else if self.manager == Some(message.object) && message.opcode == 0 {
                    self.portal_state(&message)
                } else {
                    Err(format!(
                        "unexpected private dialog event object={} opcode={}",
                        message.object, message.opcode
                    ))
                }
            }
        }
    }
}

impl<'a, F: Fn(Notice) -> Result<()>> App for Dialog<'a, F> {
    type Tag = Object;

    fn client(&mut self) -> &mut Client<Object> {
        &mut self.client
    }

    fn needs_descriptor(&self, _: &Message) -> Result<bool> {
        // The dialog's own object, the manager, carries no rights; the client
        // answers for the keymap descriptor itself.
        Ok(false)
    }

    fn descriptor_wait(&mut self) {}

    fn tick(&mut self, _now: u64) -> Result<()> {
        Ok(())
    }

    fn event(&mut self, message: Message) -> Result<()> {
        self.dispatch(message)
    }

    fn end_turn(&mut self, now: u64, _idle: bool) -> Result<()> {
        self.maintain(now)
    }

    fn draw(&mut self) -> Result<()> {
        Dialog::draw(self)
    }
}

/// Spawn the dialog worker. It reports its outcome through `notice`, whatever
/// the transport does, so a spawn failure is the only error the caller sees
/// here.
pub fn spawn<F>(config: DialogConfig, notice: F) -> Result<()>
where
    F: Fn(Notice) -> Result<()> + Send + 'static,
{
    thread::Builder::new()
        .name("td-portal-file-chooser".into())
        .spawn(move || {
            let result = run_dialog(config, &notice);
            let _ = notice(Notice::Completed(result));
        })
        .map(|_| ())
        .map_err(|error| format!("spawn FileChooser dialog worker: {error}"))
}

fn run_dialog<F: Fn(Notice) -> Result<()>>(config: DialogConfig, notice: &F) -> Result<Outcome> {
    let deadline = Instant::now()
        .checked_add(HANDSHAKE_TIMEOUT)
        .ok_or("private Wayland handshake deadline overflowed")?;
    let socket = config.socket.clone();
    let stream = bounded_connect(config.connector.clone(), deadline, move || {
        connect_blocking_until(&socket, deadline)
    })?;
    // The caller receives a clone of the connected stream; shutting it down is
    // how the portal aborts a dialog the shared client cannot see cancelled.
    let canceller = stream
        .try_clone()
        .map_err(|error| format!("clone private portal Wayland cancellation handle: {error}"))?;
    notice(Notice::Connected(canceller))?;
    let mut dialog = Dialog::new(config, stream, notice)?;
    run(&mut dialog)?;
    dialog
        .outcome
        .ok_or_else(|| "private FileChooser dialog ended without an outcome".to_string())
}

/// A time budget that has not yet elapsed, or a timeout error.
fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| "private Wayland handshake timed out".to_string())
}

/// Run `operation` on a single worker lane, bounded by `deadline`. `connector`
/// is the one-at-a-time lease: a second dialog cannot open a private
/// connection while one is in flight.
fn bounded_connect<F>(
    connector: Arc<AtomicBool>,
    deadline: Instant,
    operation: F,
) -> Result<UnixStream>
where
    F: FnOnce() -> Result<UnixStream> + Send + 'static,
{
    let lease = ConnectLease::acquire(connector)?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("td-portal-wayland-connect".into())
        .spawn(move || {
            let result = operation();
            drop(lease);
            let _ = sender.send(result);
        })
        .map_err(|error| format!("spawn the private Wayland connector: {error}"))?;
    let wait = remaining(deadline)?;
    receiver.recv_timeout(wait).map_err(|error| match error {
        mpsc::RecvTimeoutError::Timeout => {
            "private portal Wayland connect exceeded its 20-second deadline".to_string()
        }
        mpsc::RecvTimeoutError::Disconnected => {
            "private portal Wayland connector exited without a result".to_string()
        }
    })?
}

fn connect_blocking_until(path: &Path, deadline: Instant) -> Result<UnixStream> {
    let mut last = None;
    for attempt in 0..CONNECT_ATTEMPTS {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                last = Some(error);
                if attempt + 1 < CONNECT_ATTEMPTS {
                    let Some(wait) = deadline
                        .checked_duration_since(Instant::now())
                        .filter(|wait| !wait.is_zero())
                    else {
                        break;
                    };
                    thread::sleep(wait.min(Duration::from_millis(100)));
                }
            }
            Err(error) => {
                return Err(format!(
                    "connect private portal Wayland socket {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Err(format!(
        "connect private portal Wayland socket {} after {CONNECT_ATTEMPTS} attempts: {}",
        path.display(),
        last.map_or_else(|| "unknown error".to_string(), |error| error.to_string())
    ))
}

/// A single-lane lease over the shared connector flag, released on drop.
struct ConnectLease(Arc<AtomicBool>);

impl ConnectLease {
    fn acquire(active: Arc<AtomicBool>) -> Result<Self> {
        active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "the private Wayland connector is already occupied".to_string())?;
        Ok(Self(active))
    }
}

impl Drop for ConnectLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Map a raw evdev keycode and the current modifier mask to a chooser action.
/// The mapping is by keycode, not by the compositor's keymap symbols, so the
/// physical positions of Accept, Cancel and navigation are fixed regardless of
/// the map the compositor installs. Group changes and Alt/Super are refused.
fn key_action(key: u32, modifiers: u32, group: u32) -> Option<Action> {
    if group != 0 || modifiers & (MOD_ALT | MOD_LOGO) != 0 {
        return None;
    }
    if matches!(key, 28 | 96) && modifiers & MOD_CONTROL != 0 {
        return Some(Action::Accept);
    }
    if modifiers & MOD_CONTROL != 0 {
        return None;
    }
    match key {
        1 => Some(Action::Cancel),
        14 => Some(Action::Backspace),
        28 | 96 | 106 => Some(Action::Activate),
        57 => Some(Action::Toggle),
        103 => Some(Action::Previous),
        105 => Some(Action::Parent),
        108 => Some(Action::Next),
        _ => key_character(key, modifiers).map(Action::Insert),
    }
}

/// The ASCII letter a raw evdev keycode types, with shift and caps lock folded
/// in. Its own fixed QWERTY table, so filter input never depends on the
/// compositor's keymap symbols either.
fn key_character(key: u32, modifiers: u32) -> Option<char> {
    let letter = match key {
        16 => 'q',
        17 => 'w',
        18 => 'e',
        19 => 'r',
        20 => 't',
        21 => 'y',
        22 => 'u',
        23 => 'i',
        24 => 'o',
        25 => 'p',
        30 => 'a',
        31 => 's',
        32 => 'd',
        33 => 'f',
        34 => 'g',
        35 => 'h',
        36 => 'j',
        37 => 'k',
        38 => 'l',
        44 => 'z',
        45 => 'x',
        46 => 'c',
        47 => 'v',
        48 => 'b',
        49 => 'n',
        50 => 'm',
        _ => return None,
    };
    let uppercase = (modifiers & MOD_SHIFT != 0) ^ (modifiers & MOD_CAPS != 0);
    Some(if uppercase {
        letter.to_ascii_uppercase()
    } else {
        letter
    })
}

/// FNV-1a over the whole frame, the first-frame fingerprint the caller relays
/// and the boot scanner cross-checks.
fn pixel_checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn physical_keys_map_to_closed_chooser_actions() {
        assert_eq!(key_action(108, 0, 0), Some(Action::Next));
        assert_eq!(key_action(103, 0, 0), Some(Action::Previous));
        assert_eq!(key_action(105, 0, 0), Some(Action::Parent));
        assert_eq!(key_action(57, 0, 0), Some(Action::Toggle));
        assert_eq!(key_action(14, 0, 0), Some(Action::Backspace));
        assert_eq!(key_action(1, 0, 0), Some(Action::Cancel));
        assert_eq!(key_action(106, 0, 0), Some(Action::Activate));
        assert_eq!(key_action(28, 0, 0), Some(Action::Activate));
        // Control turns Return/KP-Enter into Accept and swallows the rest.
        assert_eq!(key_action(28, MOD_CONTROL, 0), Some(Action::Accept));
        assert_eq!(key_action(96, MOD_CONTROL, 0), Some(Action::Accept));
        assert_eq!(key_action(108, MOD_CONTROL, 0), None);
        // Letters type into the filter; shift and caps fold in.
        assert_eq!(key_action(19, 0, 0), Some(Action::Insert('r')));
        assert_eq!(key_action(19, MOD_SHIFT, 0), Some(Action::Insert('R')));
        assert_eq!(key_action(19, MOD_CAPS, 0), Some(Action::Insert('R')));
        assert_eq!(key_action(19, MOD_SHIFT | MOD_CAPS, 0), Some(Action::Insert('r')));
        // Alt, Super and a non-zero layout group are refused entirely.
        assert_eq!(key_action(19, MOD_ALT, 0), None);
        assert_eq!(key_action(28, MOD_LOGO, 0), None);
        assert_eq!(key_action(19, 0, 1), None);
    }

    #[test]
    fn pixel_checksum_is_stable_and_order_sensitive() {
        assert_eq!(pixel_checksum(b"pixels"), pixel_checksum(b"pixels"));
        assert_ne!(pixel_checksum(b"pixels"), pixel_checksum(b"pixelS"));
    }

    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Self {
            for attempt in 0..32u8 {
                let path = std::env::temp_dir().join(format!(
                    "td-portal-dialog-{name}-{}-{attempt}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("create dialog test directory: {error}"),
                }
            }
            panic!("exhausted dialog test directory names");
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn config(temp: &Temp, title: &str) -> DialogConfig {
        let root = temp.0.join("Downloads");
        let _ = fs::create_dir(&root);
        DialogConfig {
            socket: temp.0.join("wayland-0"),
            runtime_directory: temp.0.clone(),
            title: title.to_string(),
            parent_handle: "0123456789abcdef".into(),
            app_id: "firefox".into(),
            host_root: root,
            guest_root: PathBuf::from("/home/td/Downloads"),
            mode: Mode::OpenFile { multiple: false },
            accept_label: None,
            filter: None,
            connector: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A decoded wire event for `object`.`opcode` carrying one `u32` argument.
    fn event(object: u32, opcode: u16, arg: u32) -> Message {
        let mut body = Builder::new();
        body.u32(arg);
        let mut bytes = body.message(object, opcode).unwrap();
        td_ui::wire::take(&mut bytes).unwrap().unwrap()
    }

    #[test]
    fn keepalive_pulses_one_at_a_time_and_is_retired_on_done() {
        let temp = Temp::new("keepalive");
        let notice = |_: Notice| Ok(());
        // Keep the peer end open so the pulse's write does not break the pipe.
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut dialog = Dialog::new(config(&temp, "report"), stream, &notice).unwrap();

        // No keepalive before ten seconds.
        dialog.maintain(0).unwrap();
        assert!(dialog.pulse.is_none());
        dialog.maintain(9_999).unwrap();
        assert!(dialog.pulse.is_none());

        // At ten seconds one goes out and its callback is recorded.
        dialog.maintain(10_000).unwrap();
        let first = dialog.pulse.expect("a keepalive pulse");
        assert_eq!(dialog.last_pulse, 10_000);

        // Mark the dialog interactive so the handshake deadline no longer
        // applies: the user may read the chooser indefinitely, kept open only
        // by these pulses.
        dialog.presented_once = true;

        // No second keepalive before the interval elapses; the first stays
        // out until it is retired.
        dialog.maintain(15_000).unwrap();
        assert_eq!(dialog.pulse, Some(first));

        // The callback's `done` retires it and frees the next pulse; the
        // following `delete_id` reclaims the slot and would be a protocol error
        // were the callback not retired first.
        dialog.dispatch(event(first, 0, 0)).unwrap();
        assert!(dialog.pulse.is_none());
        dialog.dispatch(event(DISPLAY, 1, first)).unwrap();

        // A later turn sends a fresh keepalive.
        dialog.maintain(20_002).unwrap();
        assert!(dialog.pulse.is_some());
    }

    #[test]
    fn an_unanswered_keepalive_fails_closed() {
        let temp = Temp::new("keepalive-stall");
        let notice = |_: Notice| Ok(());
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut dialog = Dialog::new(config(&temp, "report"), stream, &notice).unwrap();
        // Interactive, so no handshake deadline applies; the keepalive is the
        // only liveness bound left.
        dialog.presented_once = true;
        // A keepalive goes out at ten seconds; the peer never answers, so a
        // full interval later the stall is detected and the worker fails closed
        // rather than waiting on the compositor's own receive timeout.
        dialog.maintain(10_000).unwrap();
        assert!(dialog.pulse.is_some());
        assert!(dialog.maintain(20_000).is_err());
    }

    #[test]
    fn a_stalled_handshake_trips_the_progress_deadline() {
        let temp = Temp::new("stall");
        let notice = |_: Notice| Ok(());
        let mut dialog =
            Dialog::new(config(&temp, "report"), UnixStream::pair().unwrap().0, &notice).unwrap();
        // The first turn arms the handshake deadline at t=0; twenty seconds on,
        // with no frame presented, the worker fails closed instead of hanging.
        dialog.maintain(0).unwrap();
        assert!(dialog.maintain(20_000).is_err());
    }

    #[test]
    fn caller_title_keeps_the_authenticated_prefix() {
        let temp = Temp::new("title");
        let notice = |_: Notice| Ok(());
        let empty = Dialog::new(config(&temp, ""), UnixStream::pair().unwrap().0, &notice).unwrap();
        assert_eq!(empty.window_title, "firefox — Open file");
        let maximum = "a".repeat(file_chooser::MAX_RENDERED_TITLE_BYTES - "firefox — ".len());
        let dialog =
            Dialog::new(config(&temp, &maximum), UnixStream::pair().unwrap().0, &notice).unwrap();
        assert!(dialog.window_title.starts_with("firefox — "));
        assert!(dialog.window_title.len() <= file_chooser::MAX_RENDERED_TITLE_BYTES);
    }

    #[test]
    fn stalled_connect_has_one_bounded_worker_lane() {
        let connector = Arc::new(AtomicBool::new(false));
        let (release, blocked) = mpsc::sync_channel(1);
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(50))
            .unwrap();
        let error = bounded_connect(connector.clone(), deadline, move || {
            blocked.recv().unwrap();
            Err("released stalled connector".into())
        })
        .unwrap_err();
        assert!(error.contains("20-second deadline"));

        // A second lease is refused while the first worker still holds it.
        let invoked = Arc::new(AtomicBool::new(false));
        let invoked_by_worker = invoked.clone();
        let deadline = Instant::now().checked_add(Duration::from_secs(1)).unwrap();
        let error = bounded_connect(connector.clone(), deadline, move || {
            invoked_by_worker.store(true, Ordering::Release);
            Ok(UnixStream::pair().unwrap().0)
        })
        .unwrap_err();
        assert!(error.contains("connector is already occupied"));
        assert!(!invoked.load(Ordering::Acquire));

        release.send(()).unwrap();
        for _ in 0..100 {
            if !connector.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!connector.load(Ordering::Acquire));
        let deadline = Instant::now().checked_add(Duration::from_secs(1)).unwrap();
        assert!(
            bounded_connect(connector, deadline, || Ok(UnixStream::pair().unwrap().0)).is_ok()
        );
    }
}
