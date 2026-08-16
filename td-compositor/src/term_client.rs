//! td-term's Wayland client: the handshake, the frame, and the grid it earns.
//!
//! It presents, and readiness follows the frame as §12 requires — both the
//! buffer release and the frame callback. TWO frames, because a compositor
//! cannot tile a surface it has not mapped: the first configure is zero in
//! both axes, presenting at the fallback is what maps the surface, and the
//! tile arrives in the configure after that. No child yet, so the model
//! rendered here is empty.

use crate::conn::{
    self, Connection, Globals, COMPOSITOR, KEYBOARD, REGISTRY, SEAT, SHM, SURFACE, XDG_SURFACE,
    XDG_TOPLEVEL, XDG_WM_BASE,
};
use crate::font::Font;
use crate::pty::Pty;
use crate::scene::SHM_XRGB8888;
use crate::term::Terminal;
use crate::{
    font, keys, pty, ready, render, socket, wire, MAX_HELD_KEYS, MAX_UI_DIMENSION,
    MAX_UI_FRAME_BYTES,
};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub struct Options {
    pub socket: PathBuf,
    pub ready_socket: PathBuf,
}

/// Bound on reaching a presented frame. Past this the compositor is not
/// coming, and a terminal that waits forever is one td-svc reports as down
/// without ever saying why. §12 sets it below the supervisor's 30.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// Where the session's own identity is read from. Named here rather than
/// inline so the two files the account is derived from are visible together:
/// the uid comes from the first and everything else from the second, and a
/// disagreement between them is what `current_account` refuses on.
const PROC_STATUS: &str = "/proc/self/status";
const ETC_PASSWD: &str = "/etc/passwd";

/// One past the last fixed id the TERMINAL creates: a seat and a keyboard.
/// Its `wl_pointer` is NOT among them, and that is the whole reason it takes
/// a DYNAMIC id — the pointer is created only where the seat advertises the
/// capability, so a fixed id reserved for it would be skipped on a
/// keyboard-only seat, and Wayland forbids the gap. A dynamic id is dense
/// either way, being one the connection hands out when it is actually asked
/// for. See `conn`'s note on why this is per-client.
const FIRST_DYNAMIC_ID: u32 = KEYBOARD + 1;

/// What an operator sees in a title bar. td's own compositor now KEEPS this
/// rather than discarding it, so it is the name this window will carry.
const TITLE: &str = "td terminal";

/// The grid a terminal falls back to when the compositor proposes no size.
/// 80 by 24 is what a terminfo entry, a shell prompt and anything that draws
/// a box assume when they cannot ask — so the fallback is expressed in CELLS
/// and turned into pixels by the font, rather than being a pixel constant
/// that happens to divide into some grid or other.
const DEFAULT_COLUMNS: usize = 80;
const DEFAULT_ROWS: usize = 24;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Size {
    pub width: usize,
    pub height: usize,
}

/// The pixel size of a grid this many cells across, which is what the client
/// asks for when the compositor proposes nothing.
pub fn default_size(font: &Font) -> Result<Size, String> {
    let width = DEFAULT_COLUMNS
        .checked_mul(font.width())
        .ok_or_else(|| "the default column count overflows a pixel width".to_string())?;
    let height = DEFAULT_ROWS
        .checked_mul(font.height())
        .ok_or_else(|| "the default row count overflows a pixel height".to_string())?;
    Ok(Size { width, height })
}

/// The cell grid a surface of this size holds, as a pair the readiness line
/// can carry. `grid_for_tile` is the same division the renderer clips to, and
/// `grid_size` is the same validity the winsize ioctl is held to — so a grid
/// this refuses is one nothing downstream would have accepted either.
pub fn grid(size: Size, font: &Font) -> Result<(u16, u16), String> {
    let (rows, columns) = pty::grid_for_tile(size.width, size.height, font.width(), font.height())?;
    let window = pty::grid_size(rows, columns)?;
    Ok((window.rows, window.columns))
}

/// The registry names of the globals this client bound, so that a later
/// `global_remove` naming one of them can be told from a device it never
/// asked for. A bound global that goes away leaves requests to it silently
/// ignored by the compositor, which is a terminal that stalls at its first
/// buffer rather than one that says what happened.
#[derive(Clone, Copy)]
struct Bound {
    compositor: u32,
    shm: u32,
    xdg_wm_base: u32,
    seat: u32,
}

impl Bound {
    fn interface(self, name: u32) -> Option<&'static str> {
        match name {
            _ if name == self.compositor => Some("wl_compositor"),
            _ if name == self.shm => Some("wl_shm"),
            _ if name == self.xdg_wm_base => Some("xdg_wm_base"),
            _ if name == self.seat => Some("wl_seat"),
            _ => None,
        }
    }
}

/// Bind the four globals a terminal needs. The `wl_seat` is one of them now:
/// a keymap arrives through a keyboard, and a keyboard is a seat's to give.
fn bind_globals(connection: &mut Connection) -> Result<Bound, String> {
    let globals = conn::discover_globals(connection)?;
    let (compositor_name, compositor_version) =
        Globals::require(globals.compositor(), "wl_compositor", 4, 4)?;
    let (shm_name, shm_version) = Globals::require(globals.shm(), "wl_shm", 1, 1)?;
    let (xdg_name, xdg_version) = Globals::require(globals.xdg_wm_base(), "xdg_wm_base", 1, 1)?;
    let (seat_name, seat_version) = Globals::require(
        globals.seat(),
        "wl_seat",
        SEAT_VERSION_MINIMUM,
        SEAT_VERSION_MAXIMUM,
    )?;
    conn::bind(
        connection,
        compositor_name,
        "wl_compositor",
        compositor_version,
        COMPOSITOR,
    )?;
    conn::bind(connection, shm_name, "wl_shm", shm_version, SHM)?;
    conn::bind(
        connection,
        xdg_name,
        "xdg_wm_base",
        xdg_version,
        XDG_WM_BASE,
    )?;
    conn::bind(connection, seat_name, "wl_seat", seat_version, SEAT)?;
    Ok(Bound {
        compositor: compositor_name,
        shm: shm_name,
        xdg_wm_base: xdg_name,
        seat: seat_name,
    })
}

/// `XDG_TOPLEVEL_STATE_ACTIVATED`. Pinned by value, as every other protocol
/// number here is: this one decides whether the cursor is drawn as holding
/// the keyboard, so a wrong constant is a terminal whose cursor never claims
/// focus — with nothing failing anywhere.
const XDG_STATE_ACTIVATED: u32 = 4;

/// `WL_SEAT_CAPABILITY_KEYBOARD`, pinned by value like every other protocol
/// number here: a wrong bit is a terminal that never asks for a keyboard, or
/// one that asks the moment a pointer appears.
const SEAT_KEYBOARD: u32 = 2;

/// `WL_SEAT_CAPABILITY_POINTER`, pinned for the same reason: a wrong bit is a
/// terminal that asks for a pointer when a keyboard appears, and Wayland makes
/// a `get_pointer` on a seat without the capability a protocol error.
const SEAT_POINTER: u32 = 1;

/// Lines one detent scrolls. Three is the convention every other terminal
/// follows, and the reason it is not one is that a wheel is turned in flicks:
/// a notch per line makes reaching the top of a screenful a dozen turns.
/// A PAGE is what the keys do, and a wheel that paged would overshoot.
const LINES_PER_NOTCH: i32 = 3;

/// `WL_POINTER_AXIS_VERTICAL_SCROLL`. A terminal has no horizontal
/// scrollback, so the other axis is read only to be ignored — and it must be
/// READ rather than assumed absent, since a tilting wheel sends both and
/// counting them together would scroll a sideways flick up the history.
const POINTER_AXIS_VERTICAL: u32 = 0;

/// The `wl_seat` version range this binds, and the wheel depends on BOTH ends
/// of it rather than on either alone. The floor is what guarantees a
/// `wl_pointer.frame` — the event the accumulated notches are applied on — and
/// an `axis_discrete` beside every wheel axis; below 5 neither exists, so the
/// wheel would accumulate forever and never scroll, with nothing failing. The
/// ceiling is what keeps `axis_value120` and `axis_relative_direction` from
/// arriving, since this client has no arm for either and its catch-all is
/// fatal. Named so those are relations rather than two numbers in a call
/// nobody connects to this module.
const SEAT_VERSION_MINIMUM: u32 = 5;
const SEAT_VERSION_MAXIMUM: u32 = 7;
/// Checked where it cannot be skipped, as the server's deprecation bound is:
/// both are constants, so a test asserting it would only ever be true.
const _: () = assert!(SEAT_VERSION_MINIMUM >= 5 && SEAT_VERSION_MAXIMUM <= 7);

/// `WL_KEYBOARD_KEY_STATE_RELEASED` and `_PRESSED`, the only two a version-1
/// wl_keyboard may send. Checked rather than merely consumed because the
/// next landing turns this word into a keystroke or the absence of one, and
/// a third value would decide which by falling through.
const KEY_RELEASED: u32 = 0;
const KEY_PRESSED: u32 = 1;

/// The frame in flight. §12 has readiness follow BOTH answers, because they
/// mean different things: the release says the compositor is done reading the
/// buffer, and the callback says the frame reached the screen. A terminal that
/// announced itself on either alone would be announcing something it cannot
/// see.
struct Frame {
    buffer: u32,
    callback: u32,
    released: bool,
    presented: bool,
}

impl Frame {
    /// Named for the state rather than for one of its halves, so it does
    /// not shadow the `presented` field it reads.
    fn complete(&self) -> bool {
        self.released && self.presented
    }
}

/// What a frame was drawn FOR. The size is the obvious half; the activation
/// state is the other, because it decides how the cursor is drawn — so a
/// configure that only takes focus away still needs a new picture, and a
/// redraw decision made on the size alone would never make one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Drawn {
    size: Size,
    activated: bool,
}

/// What the client knows about its surface between configures.
///
/// The toplevel configure carries the size and the SURFACE configure is where
/// it takes effect, so a proposal is only ever adopted at the second — which
/// is also the only one that is acknowledged. Two events, one transition.
struct Surface {
    bound: Bound,
    proposed: Option<Size>,
    current: Option<Size>,
    xrgb: bool,
    activated: bool,
    /// The activation state the pending configure carries. Double-buffered
    /// with the size for the same reason: both arrive on the toplevel
    /// configure and xdg-shell applies the whole configure at the surface one.
    proposed_activated: Option<bool>,
    /// Whether the pending proposal actually CHOSE a size. The compositor's
    /// first configure is zero in both axes — it cannot know a tile for a
    /// surface it has not mapped — so a terminal that stopped there would
    /// announce its own fallback. Presenting is what maps it; this is how the
    /// configure that follows is told from the one that preceded it.
    proposed_selected: bool,
    layout_configured: bool,
    /// An acknowledged configure that has not been applied yet. xdg-shell
    /// makes `ack_configure` take effect on the surface commit that FOLLOWS
    /// it, so acknowledging is only half of answering one.
    needs_commit: bool,
    /// What the frame in flight was drawn for, which is not always what the
    /// surface now wants: a configure can arrive while a frame is in the air.
    drawn: Option<Drawn>,
    /// The grid the PTY was last set to, which is the grid the published
    /// readiness line names. On the surface because `adopt_size` is what
    /// establishes it and both loops go through there.
    cells: Option<(u16, u16)>,
    /// The model changed under a picture that did not. `drawn` answers "is
    /// the frame for the right SIZE and focus"; nothing in it can answer "is
    /// it for the right CONTENTS", because the contents are not a size.
    stale: bool,
    /// The last capability word the seat announced, retained rather than
    /// consumed: a seat may announce a keyboard and later withdraw it, and a
    /// terminal whose seat has no keyboard is one nobody can type into
    /// however well its keymap verified. `None` until the seat speaks, which
    /// is also what stops an unsolicited keymap from standing in for one.
    seat_capabilities: Option<u32>,
    /// Whether `get_keyboard` has been sent. A seat may re-announce its
    /// capabilities, and a second one into the same object id is a protocol
    /// error rather than a second keyboard.
    keyboard_requested: bool,
    /// The `wl_pointer`'s id, and the once-only flag in one: a seat may
    /// re-announce, and a second `get_pointer` is both a wasted id and — into
    /// the same one — a protocol error. `None` until a seat offers the
    /// capability, which is also how a keyboard-only seat is served.
    pointer: Option<u32>,
    /// Detents accumulated since the last `wl_pointer.frame`. A frame is the
    /// transaction, so a report that turned the wheel is applied once when it
    /// closes rather than per axis event — which is also what keeps a tilting
    /// wheel from repainting twice for one flick.
    pending_notches: i32,
    /// Whether the compositor's keymap has been received and matched against
    /// td's pinned one. §11 makes this a precondition of the child starting.
    keymap_verified: bool,
    /// The effective XKB modifier mask — depressed, latched and locked folded
    /// together, which is what `keys` reads. Held rather than recomputed
    /// because a key event carries no modifiers of its own.
    modifiers: u32,
    /// Whether the current non-zero group has been reported. Cleared when
    /// the group changes, so returning to td's own map and leaving it again
    /// reports the second departure too.
    group_reported: bool,
    /// The keymap group the last modifiers event selected. td's pinned map
    /// has exactly one, so a non-zero group is a layout whose key meanings
    /// this table does not describe — and translating it against group 0
    /// would send the wrong bytes rather than none.
    group: u32,
    /// The terminal modes that pick between two spellings of the same key,
    /// refreshed from the model before each dispatch: `dispatch` translates,
    /// and the model that knows the mode is not its to reach.
    modes: keys::Modes,
    /// One translated key press, waiting for `serve_event` to route it to the
    /// child. A dispatch handles one message and so yields at most one.
    pending_input: Option<keys::Sequence>,
    /// The key event `dispatch` last read, as (code, pressed), waiting for a
    /// caller that has a CLOCK. `dispatch` has none and should not: it is
    /// driven by tests that inject events, not time.
    pending_key: Option<(u16, bool)>,
    /// td-term's scrollback viewport: which line is being looked at, not a
    /// distance from a bottom that moves.
    viewport: keys::Viewport,
    /// The history the viewport is read against, refreshed from the model
    /// beside `modes` because `dispatch` cannot reach it either.
    history: keys::Scrollback,
    /// The auto-repeat state machine. Its timings come from the compositor's
    /// `repeat_info` rather than from td's own constants, because §11 has the
    /// server publish them and a client that guessed would drift from it.
    repeat: keys::Repeat,
    live_buffers: BTreeSet<u32>,
    live_callbacks: BTreeSet<u32>,
    frame: Option<Frame>,
}

impl Surface {
    fn new(bound: Bound) -> Surface {
        Surface {
            bound,
            proposed: None,
            current: None,
            xrgb: false,
            activated: false,
            proposed_activated: None,
            proposed_selected: false,
            layout_configured: false,
            needs_commit: false,
            drawn: None,
            cells: None,
            stale: false,
            seat_capabilities: None,
            keyboard_requested: false,
            pointer: None,
            pending_notches: 0,
            keymap_verified: false,
            modifiers: 0,
            group: 0,
            group_reported: false,
            modes: keys::Modes::default(),
            pending_input: None,
            pending_key: None,
            viewport: keys::Viewport::new(),
            history: keys::Scrollback::default(),
            repeat: keys::Repeat::new(),
            live_buffers: BTreeSet::new(),
            live_callbacks: BTreeSet::new(),
            frame: None,
        }
    }

    /// Handle one event. `Ok(true)` means a configure completed, so the
    /// surface now has a size the caller can act on.
    fn dispatch(
        &mut self,
        connection: &mut Connection,
        message: &wire::Message,
        fallback: Size,
    ) -> Result<bool, String> {
        if message.object == REGISTRY && (message.opcode == 0 || message.opcode == 1) {
            // `wl_registry` has no destroy request, so it stays live for the
            // whole session and a monitor or input device arriving after
            // discovery is delivered HERE. The terminal has everything it
            // asked for, so a later global is not its business — but dying on
            // one would make an ordinary hotplug take the terminal down. It is
            // still PARSED, because every other arm validates what it consumes
            // and a malformed event is a broken compositor either way.
            let mut args = wire::Cursor::new(&message.payload);
            let name = args.u32()?;
            if message.opcode == 0 {
                args.string()?;
                args.u32()?;
            }
            args.finish()?;
            // A global this client BOUND going away is different: the
            // compositor ignores every later request to that object, so the
            // terminal would stall at its first buffer with nothing to report.
            if message.opcode == 1 {
                if let Some(interface) = self.bound.interface(name) {
                    return Err(format!(
                        "compositor withdrew {interface} while it was in use"
                    ));
                }
            }
            return Ok(false);
        }
        if message.object == SHM && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            if args.u32()? == SHM_XRGB8888 {
                self.xrgb = true;
            }
            args.finish()?;
            return Ok(false);
        }
        if message.object == XDG_TOPLEVEL && message.opcode == 0 {
            let (proposed, activated, selected) = self.toplevel_size(message, fallback)?;
            self.proposed = Some(proposed);
            self.proposed_activated = Some(activated);
            self.proposed_selected = selected;
            return Ok(false);
        }
        if message.object == XDG_TOPLEVEL && message.opcode == 1 {
            return Err("compositor requested that the terminal close".into());
        }
        if message.object == XDG_SURFACE && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            let serial = args.u32()?;
            args.finish()?;
            let mut ack = wire::Builder::new();
            ack.u32(serial);
            connection.send(XDG_SURFACE, 4, ack)?;
            // A bare surface configure REUSES the last applied size rather
            // than falling back: the compositor is confirming what it already
            // proposed, not withdrawing it.
            self.current = Some(self.proposed.take().or(self.current).unwrap_or(fallback));
            if let Some(activated) = self.proposed_activated.take() {
                self.activated = activated;
            }
            if std::mem::take(&mut self.proposed_selected) {
                self.layout_configured = true;
            }
            self.needs_commit = true;
            return Ok(true);
        }
        if message.object == SEAT {
            let mut args = wire::Cursor::new(&message.payload);
            match message.opcode {
                // Capabilities. The keyboard is bound the FIRST time one is
                // announced and never again: a seat may re-announce, and a
                // second `get_keyboard` into the same id is a protocol error.
                0 => {
                    let capabilities = args.u32()?;
                    args.finish()?;
                    self.seat_capabilities = Some(capabilities);
                    if capabilities & SEAT_KEYBOARD != 0 && !self.keyboard_requested {
                        let mut keyboard = wire::Builder::new();
                        keyboard.u32(KEYBOARD);
                        connection.send(SEAT, 1, keyboard)?;
                        self.keyboard_requested = true;
                    }
                    if capabilities & SEAT_POINTER != 0 && self.pointer.is_none() {
                        let id = connection.allocate_id()?;
                        let mut pointer = wire::Builder::new();
                        pointer.u32(id);
                        connection.send(SEAT, 0, pointer)?;
                        self.pointer = Some(id);
                    }
                }
                // The seat's name, which nothing here reads.
                1 => {
                    args.string()?;
                    args.finish()?;
                }
                _ => {
                    return Err(format!(
                        "unexpected wl_seat event opcode={}",
                        message.opcode
                    ))
                }
            }
            return Ok(false);
        }
        if message.object == KEYBOARD {
            let mut args = wire::Cursor::new(&message.payload);
            match message.opcode {
                0 => {
                    let format = args.u32()?;
                    let size = args.u32()?;
                    args.finish()?;
                    let file = connection.take_fd("wl_keyboard.keymap")?;
                    conn::verify_keymap(&file, format, size)?;
                    self.keymap_verified = true;
                }
                // Focus in and out. The renderer's notion of focus comes from
                // `xdg_toplevel`'s activated state rather than from here, so
                // what these arms owe is the modifier reset below. Parsed
                // rather than
                // skipped because every other arm here validates what it
                // consumes, and a wl_keyboard that entered a surface this
                // client does not own is a compositor confusing it with
                // another client.
                1 => {
                    args.u32()?;
                    let surface = args.u32()?;
                    if surface != SURFACE {
                        return Err(format!("wl_keyboard entered unexpected surface {surface}"));
                    }
                    let byte_count = usize::try_from(args.u32()?)
                        .map_err(|_| "wl_keyboard key array size escaped usize".to_string())?;
                    if !byte_count.is_multiple_of(4) || byte_count / 4 > MAX_HELD_KEYS {
                        return Err(format!(
                            "wl_keyboard key array has invalid length {byte_count}"
                        ));
                    }
                    for _ in 0..byte_count / 4 {
                        args.u32()?;
                    }
                    args.finish()?;
                }
                2 => {
                    args.u32()?;
                    let surface = args.u32()?;
                    if surface != SURFACE {
                        return Err(format!("wl_keyboard left unexpected surface {surface}"));
                    }
                    args.finish()?;
                    // Leaving lifts every key, modifiers among them, which the
                    // protocol obliges a client to assume. td's server re-sends
                    // modifiers right after each enter and cannot interleave a
                    // key between the two, so nothing here could read a stale
                    // Ctrl — but a client that kept one would be wrong, and the
                    // cost of not keeping it is a word.
                    self.modifiers = 0;
                    self.repeat.cancel();
                }
                // key: serial, time, key, state.
                3 => {
                    args.u32()?;
                    args.u32()?;
                    let code = args.u32()?;
                    let pressed = match args.u32()? {
                        KEY_RELEASED => false,
                        KEY_PRESSED => true,
                        state => return Err(format!("wl_keyboard key has invalid state {state}")),
                    };
                    args.finish()?;
                    if let Ok(code) = u16::try_from(code) {
                        self.pending_key = Some((code, pressed));
                    }
                    if pressed {
                        self.translate(code)?;
                    }
                }
                // modifiers: serial, then depressed, latched, locked and the
                // group. The three masks fold into the one `keys` reads;
                // nothing here distinguishes a held Shift from a latched one.
                4 => {
                    args.u32()?;
                    let depressed = args.u32()?;
                    let latched = args.u32()?;
                    let locked = args.u32()?;
                    let group = args.u32()?;
                    args.finish()?;
                    // Staged like the three masks above: nothing is recorded
                    // until the whole message has been read.
                    if group != self.group {
                        self.group = group;
                        self.group_reported = false;
                        // Same argument as the modifier change below, and
                        // sharper: a key armed under td's map would go on
                        // sending ITS bytes after a switch, which is the
                        // wrong-key outcome a fresh press in a foreign group
                        // is refused to avoid.
                        self.repeat.cancel();
                    }
                    let folded = depressed | latched | locked;
                    if folded != self.modifiers {
                        // The sequence armed under the old modifiers is not
                        // the one the key would send now.
                        self.repeat.cancel();
                    }
                    self.modifiers = folded;
                }
                // repeat_info: rate and delay, the version-4 event §11's
                // repeat design reads its timings out of. Parsed and dropped
                // like the rest until the landing that repeats a key; it has
                // an arm at all because the catch-all below is fatal, so a
                // seat bound high enough to send this must expect it.
                5 => {
                    let rate = args.i32()?;
                    let delay = args.i32()?;
                    args.finish()?;
                    self.repeat.retime(&repeat_from(rate, delay));
                }
                _ => {
                    return Err(format!(
                        "unexpected wl_keyboard event opcode={}",
                        message.opcode
                    ))
                }
            }
            return Ok(false);
        }
        if Some(message.object) == self.pointer {
            let mut args = wire::Cursor::new(&message.payload);
            match message.opcode {
                // enter and leave, whose SURFACE is checked for the reason
                // the keyboard's is twenty lines up: a mismatch means the
                // compositor has confused this client with another, and a
                // terminal acting on another window's pointer would be acting
                // on that confusion rather than reporting it. Motion and
                // button carry no surface and are dropped — this client acts
                // on neither, and they have arms because the catch-all below
                // is fatal.
                0 => {
                    args.u32()?;
                    let object = args.u32()?;
                    if object != SURFACE {
                        return Err(format!("wl_pointer entered unexpected surface {object}"));
                    }
                    args.i32()?;
                    args.i32()?;
                    args.finish()?;
                }
                1 => {
                    args.u32()?;
                    let object = args.u32()?;
                    if object != SURFACE {
                        return Err(format!("wl_pointer left unexpected surface {object}"));
                    }
                    args.finish()?;
                }
                2 => {
                    args.u32()?;
                    args.i32()?;
                    args.i32()?;
                    args.finish()?;
                }
                3 => {
                    args.u32()?;
                    args.u32()?;
                    args.u32()?;
                    args.u32()?;
                    args.finish()?;
                }
                // axis: the DISTANCE, which this client does not read. The
                // notch count comes from `axis_discrete` instead, that being
                // the event which carries it — turning a distance back into
                // notches would need the compositor's own units-per-detent, a
                // number no client is given. Treating a bare axis as one notch
                // would be worse than ignoring it: a smooth-scrolling source
                // sends no discrete at all and many small axis events, and
                // each would become a whole notch. Nothing is lost by the
                // silence, since the seat is required at version 5 or above
                // and a wheel there always carries its count.
                4 => {
                    args.u32()?;
                    args.u32()?;
                    args.i32()?;
                    args.finish()?;
                }
                // frame: the transaction closes, so the wheel is applied.
                5 => {
                    args.finish()?;
                    let notches = std::mem::take(&mut self.pending_notches);
                    self.scroll_wheel(notches);
                }
                // axis_source and axis_stop, both parsed and dropped: this
                // terminal treats every scroll the same, and a wheel sends no
                // stop. axis_discrete is the one that means something.
                6 => {
                    args.u32()?;
                    args.finish()?;
                }
                7 => {
                    args.u32()?;
                    args.u32()?;
                    args.finish()?;
                }
                8 => {
                    let axis = args.u32()?;
                    let discrete = args.i32()?;
                    args.finish()?;
                    if axis == POINTER_AXIS_VERTICAL {
                        self.pending_notches = self.pending_notches.saturating_add(discrete);
                    }
                }
                _ => {
                    return Err(format!(
                        "unexpected wl_pointer event opcode={}",
                        message.opcode
                    ))
                }
            }
            return Ok(false);
        }
        if self.live_buffers.contains(&message.object) && message.opcode == 0 {
            wire::Cursor::new(&message.payload).finish()?;
            self.live_buffers.remove(&message.object);
            if let Some(frame) = self.frame.as_mut() {
                if frame.buffer == message.object {
                    frame.released = true;
                }
            }
            // The buffer is ours again; the pool it came from is long gone, so
            // destroying it is what frees the mapping.
            connection.send(message.object, 0, wire::Builder::new())?;
            return Ok(false);
        }
        if self.live_callbacks.contains(&message.object) && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            args.u32()?;
            args.finish()?;
            self.live_callbacks.remove(&message.object);
            if let Some(frame) = self.frame.as_mut() {
                if frame.callback == message.object {
                    frame.presented = true;
                }
            }
            return Ok(false);
        }
        Err(format!(
            "unexpected Wayland event object={} opcode={}",
            message.object, message.opcode
        ))
    }

    /// Discharge what an acknowledgement owes. A configure asking for a size
    /// already drawn produces no frame, and therefore no commit, so the
    /// compositor would have its configure acknowledged and never applied —
    /// which is exactly what a chosen tile equal to the fallback looks like.
    /// A bare commit says the pixels already standing are the answer.
    fn commit_configure(&mut self, connection: &mut Connection) -> Result<(), String> {
        if !std::mem::take(&mut self.needs_commit) {
            return Ok(());
        }
        connection.send(SURFACE, 6, wire::Builder::new())
    }

    /// What the surface would have to be showing to be up to date: the size
    /// it holds, drawn with the focus it holds. `None` before any configure,
    /// which is also when there is nothing it could be showing.
    fn wanted(&self) -> Option<Drawn> {
        self.current.map(|size| Drawn {
            size,
            activated: self.activated,
        })
    }

    /// Whether a frame is out that the compositor has not finished with.
    /// Drawing another would stack a second wl_shm pool on it for a picture
    /// it has not shown yet, which is what makes a burst of configures a
    /// memory leak rather than a redraw.
    fn frame_in_flight(&self) -> bool {
        self.frame.as_ref().is_some_and(|frame| !frame.complete())
    }

    /// Everything that must be true before the terminal is a terminal: a
    /// frame the compositor chose the size of, a seat that still offers a
    /// keyboard, and a keymap matched against td's own. §11 makes the keymap
    /// a precondition of CHILD CREATION — "a mismatch closes the client
    /// before child creation" — and everything past this leads to a shell,
    /// so a shell whose keyboard was never verified is one nobody should be
    /// able to type into.
    ///
    /// A function rather than the loop's condition for the reason `ready` is
    /// one: a condition written inline is a condition no test can hold to
    /// one of its parts. All three are bounded by the same handshake
    /// deadline, so a compositor that announces a seat and never sends a
    /// keymap fails rather than hangs.
    fn presented(&self) -> bool {
        self.ready() && self.has_keyboard() && self.keymap_verified
    }

    /// Turn one pressed evdev code into the bytes a child expects, or into
    /// nothing. `keys` owns every rule; what lives here is the decision NOT
    /// to translate at all: a group this keymap does not have would otherwise
    /// be read against group 0's table and send bytes for a different key.
    ///
    /// A scroll action moves the VIEW and sends nothing; a byte action sends
    /// and also returns the view to the live bottom, which is §10's rule and
    /// what stops typing echoing where nobody can see it. `viewing` is read
    /// rather than assumed because End means one thing with the view open and
    /// another at the bottom.
    fn translate(&mut self, code: u32) -> Result<(), String> {
        // Silent rather than fatal, for the reason `keys` answers `Silent` to
        // any code its table lacks: no evdev code reaches u16, so this is a
        // broken compositor, and taking the operator's shell down over one
        // key is a worse answer than ignoring it.
        let Ok(code) = u16::try_from(code) else {
            return Ok(());
        };
        if self.group != 0 {
            // Said once per group, not once per key: a silent refusal is
            // indistinguishable from a terminal that has stopped responding,
            // and a line per keystroke would be its own kind of unusable.
            if !self.group_reported {
                self.group_reported = true;
                eprintln!(
                    "td-term: keymap group {} is not td's; keys in it send nothing",
                    self.group
                );
            }
            return Ok(());
        }
        let viewing = self.viewport.viewing(self.history);
        let action = keys::action(code, self.modifiers, self.modes, viewing);
        match action {
            keys::Action::Bytes(sequence) => {
                self.pending_input = Some(sequence);
                // §10: ordinary input returns to the live bottom. Typing at a
                // scrolled-back screen otherwise echoes where nobody can see
                // it, and the reply lands there too.
                self.scroll(&action);
            }
            keys::Action::Scroll(_) => {
                self.scroll(&action);
            }
            keys::Action::Silent => {}
        }
        Ok(())
    }

    /// Move the view as `action` says, and ask for a frame only if it
    /// actually moved: PageUp at the top of history clamps to where it
    /// already was, and repainting the same lines is work the compositor
    /// does for nothing.
    fn scroll(&mut self, action: &keys::Action) {
        let rows = self.cells.map_or(0, |(rows, _)| usize::from(rows));
        let before = self.viewport.offset(self.history);
        self.viewport.apply(action, rows, self.history);
        self.stale |= self.viewport.offset(self.history) != before;
    }

    /// Move the view by a wheel's worth. `notches` is the PROTOCOL's sign,
    /// where positive is a downward movement of the surface's own content —
    /// so it scrolls toward the live bottom, and a wheel turned away from the
    /// operator, which arrives negative, goes back into history.
    fn scroll_wheel(&mut self, notches: i32) {
        if notches == 0 {
            return;
        }
        let before = self.viewport.offset(self.history);
        self.viewport.by_lines(
            notches.saturating_neg().saturating_mul(LINES_PER_NOTCH),
            self.history,
        );
        self.stale |= self.viewport.offset(self.history) != before;
    }

    /// Whether the seat's LATEST word still claims a keyboard. Asked here
    /// rather than remembered from the request, because the two can differ:
    /// a seat that announces a keyboard, has one taken, and then announces
    /// none leaves `keyboard_requested` true and a verified keymap behind it.
    fn has_keyboard(&self) -> bool {
        self.seat_capabilities
            .is_some_and(|capabilities| capabilities & SEAT_KEYBOARD != 0)
    }

    /// A frame the compositor has both released and presented, drawn FOR
    /// what the surface now holds, at a size the compositor chose. All three
    /// matter and the middle one is easy to lose — a configure can arrive
    /// while a frame is in the air, and the frame that comes back is then a
    /// picture of the wrong thing.
    fn ready(&self) -> bool {
        // `layout_configured` means a configure chose a size, so `wanted` is
        // `Some` here and the comparison cannot be two `None`s agreeing.
        self.layout_configured
            && self.drawn == self.wanted()
            && self.frame.as_ref().is_some_and(Frame::complete)
    }

    /// A configure of zero in either axis is the compositor declining to
    /// choose, not a zero-sized window; each axis declines independently, so
    /// the fallback fills in per axis rather than wholesale.
    fn toplevel_size(
        &self,
        message: &wire::Message,
        fallback: Size,
    ) -> Result<(Size, bool, bool), String> {
        let mut args = wire::Cursor::new(&message.payload);
        let width = args.i32()?;
        let height = args.i32()?;
        if width < 0 || height < 0 {
            return Err(format!(
                "compositor configured a negative terminal size {width}x{height}"
            ));
        }
        let states = usize::try_from(args.u32()?)
            .map_err(|_| "XDG state array length overflow".to_string())?;
        if !states.is_multiple_of(4) {
            return Err(format!("XDG state array has invalid length {states}"));
        }
        let mut activated = false;
        for _ in 0..states / 4 {
            if args.u32()? == XDG_STATE_ACTIVATED {
                activated = true;
            }
        }
        args.finish()?;
        let width = usize::try_from(width)
            .map_err(|_| "configured terminal width escaped usize".to_string())?;
        let height = usize::try_from(height)
            .map_err(|_| "configured terminal height escaped usize".to_string())?;
        let current = self.current.unwrap_or(fallback);
        Ok((
            Size {
                width: if width == 0 { current.width } else { width },
                height: if height == 0 { current.height } else { height },
            },
            activated,
            width != 0 || height != 0,
        ))
    }
}

/// What a compositor-supplied size must satisfy before ANYTHING is allocated
/// for it — the demo's three bounds, and for its reason: a configure comes
/// from outside, and an allocation too large to serve ABORTS rather than
/// returning an error this crate could propagate. The server refuses an
/// oversized pool, but only after the client has already tried to allocate.
/// Called before the terminal model is built as well as before the pixels
/// are, so the bound covers the first allocation rather than the last.
fn frame_bytes(size: Size) -> Result<usize, String> {
    if size.width == 0 || size.height == 0 {
        return Err(format!(
            "terminal surface {}x{} has no area",
            size.width, size.height
        ));
    }
    if size.width > MAX_UI_DIMENSION || size.height > MAX_UI_DIMENSION {
        return Err(format!(
            "terminal surface {}x{} exceeds {MAX_UI_DIMENSION}",
            size.width, size.height
        ));
    }
    let bytes = size
        .width
        .checked_mul(size.height)
        .and_then(|count| count.checked_mul(render::BYTES_PER_PIXEL))
        .ok_or_else(|| {
            format!(
                "terminal surface {}x{} overflows a byte count",
                size.width, size.height
            )
        })?;
    if bytes > MAX_UI_FRAME_BYTES {
        return Err(format!(
            "terminal surface needs {bytes} bytes, exceeding {MAX_UI_FRAME_BYTES}"
        ));
    }
    Ok(bytes)
}

/// How far back the next frame is drawn, read against the terminal being
/// DRAWN rather than the client's cached history: this is the moment the
/// picture is decided, and an offset from a stale reading would draw a line
/// the model no longer has there. A function so the composition is testable —
/// inline it is a line that can be replaced by zero with every test green.
fn draw_offset(surface: &Surface, terminal: &Terminal) -> usize {
    surface.viewport.offset(terminal.scrollback())
}

fn build_pixels(
    size: Size,
    terminal: &Terminal,
    font: &Font,
    palette: &render::Palette,
    focused: bool,
    viewport: usize,
) -> Result<Vec<u8>, String> {
    let bytes = frame_bytes(size)?;
    let mut pixels = vec![0u8; bytes];
    // No cursor override: `render.rs` shifts the cursor down by the viewport
    // and drops it once it falls off the bottom, so a scrolled-back view
    // already draws it on the right cell or not at all.
    let snapshot = render::Snapshot::new(terminal, focused, false).scrolled_back(viewport);
    render::render(
        &snapshot,
        palette,
        font,
        &mut pixels,
        size.width,
        size.height,
    )?;
    Ok(pixels)
}

/// Render the terminal into a new buffer and put it on the surface. The ids
/// are recorded so the two answers that follow can be told from an event
/// about some other object.
fn commit_frame(
    connection: &mut Connection,
    surface: &mut Surface,
    directory: &Path,
    size: Size,
    terminal: &Terminal,
    font: &Font,
    palette: &render::Palette,
) -> Result<(), String> {
    if !surface.xrgb {
        return Err("compositor did not advertise wl_shm XRGB8888".into());
    }
    let viewport = draw_offset(surface, terminal);
    let pixels = build_pixels(size, terminal, font, palette, surface.activated, viewport)?;
    let (buffer, callback) = conn::attach_frame(
        connection,
        directory,
        "td-term",
        &pixels,
        size.width,
        size.height,
    )?;
    if !surface.live_buffers.insert(buffer) {
        return Err(format!(
            "terminal buffer object {buffer} was reused while live"
        ));
    }
    if !surface.live_callbacks.insert(callback) {
        return Err(format!(
            "terminal callback object {callback} was reused while live"
        ));
    }
    surface.frame = Some(Frame {
        buffer,
        callback,
        released: false,
        presented: false,
    });
    // `attach_frame` ends in a `wl_surface.commit`, which is the commit any
    // outstanding acknowledgement was waiting for.
    surface.needs_commit = false;
    Ok(())
}

/// Everything `run` holds once the terminal is up. A struct rather than a
/// tuple because the tuple reached five on the landing that added the PTY,
/// which is where a reviewer said it would stop reading.
pub struct Prepared {
    surface: Surface,
    pub pty: Pty,
    terminal: Terminal,
    pub cells: (u16, u16),
    /// Carried out of startup rather than created with the writer: it may
    /// already hold keys struck between the surface mapping and the child
    /// existing.
    input: Arc<pty::Input>,
}

impl Prepared {
    /// The tile the surface settled on. Read off the surface rather than
    /// carried beside it, so the two cannot disagree.
    #[cfg(test)]
    pub fn size(&self) -> Result<Size, String> {
        self.surface
            .current
            .ok_or_else(|| "the terminal presented without a size".to_string())
    }
}

/// The size changed. Bound it, work out the grid, tell the PTY and verify it
/// took, reflow the model, and draw — in that order, which §12 fixes: the
/// child learns the new grid BEFORE the pixels change, so a program redrawing
/// on the size change never paints for a grid the surface no longer has.
///
/// The first frame runs this too. A terminal's opening size is just the first
/// size it was given, so `model` is what says whether there is anything to
/// reflow yet; everything after that is one path.
fn adopt_size(
    connection: &mut Connection,
    surface: &mut Surface,
    model: &mut Option<Terminal>,
    session: &Session,
) -> Result<(), String> {
    // The size comes off the surface, not from the caller: `redraw` reads it
    // there too, and a size passed in beside one read out is two answers to
    // one question.
    let Some(size) = surface.current else {
        return Ok(());
    };
    // Before the model, not only before the pixels: `build_pixels` checks
    // this again, but by then `Terminal::new` has already allocated for a
    // size nothing bounded. Deleting this line reds nothing, because the
    // model's own limits keep it to a bounded transient that returns `Err` —
    // the position is a defence in depth, and the comment is what holds it.
    frame_bytes(size)?;
    let (rows, columns) = grid(size, session.font)?;
    let window = session
        .pty
        .resize(usize::from(rows), usize::from(columns))?;
    let (rows, columns) = (usize::from(window.rows), usize::from(window.columns));
    match model.as_mut() {
        // Reflowed, NOT rebuilt: a `Terminal::new` here would leave the
        // screen intact only because it is empty today, and would silently
        // erase a session's scrollback the moment a child writes to one.
        Some(terminal) => terminal.resize(rows, columns)?,
        None => {
            *model = Some(Terminal::new(rows, columns)?);
        }
    }
    surface.cells = Some((window.rows, window.columns));
    redraw(connection, surface, model, session)
}

/// Put what the model says on the screen, at whatever size the surface holds.
/// This is the path OUTPUT takes: nothing about the geometry changed and only
/// the contents did, so there is nothing to tell the PTY and nothing to
/// reflow.
fn redraw(
    connection: &mut Connection,
    surface: &mut Surface,
    model: &Option<Terminal>,
    session: &Session,
) -> Result<(), String> {
    let Some(wanted) = surface.wanted() else {
        return Ok(());
    };
    let terminal = model
        .as_ref()
        .ok_or_else(|| "the terminal has no model to draw".to_string())?;
    commit_frame(
        connection,
        surface,
        session.directory,
        wanted.size,
        terminal,
        session.font,
        session.palette,
    )?;
    surface.drawn = Some(wanted);
    surface.stale = false;
    Ok(())
}

/// What a terminal session is, apart from what is on the screen: where its
/// wl_shm files go, the terminal its child will be given, and what a frame is
/// drawn with. None of the four changes once the terminal is up and adopting
/// a size needs all four — three to draw with and the PTY to tell — so they
/// travel as one rather than as four arguments that could be paired wrongly.
struct Session<'a> {
    directory: &'a Path,
    pty: &'a Pty,
    font: &'a Font,
    palette: &'a render::Palette,
}

/// What the terminal's main loop serves. One channel, because a loop cannot
/// block in two places and every producer is a thread that blocks in one —
/// which is what §12 means by "bounded messages to one main loop". The
/// Wayland reader is the first producer; the child's output and its exit are
/// the ones the next landing adds.
enum Event {
    Wayland(wire::Message),
    /// Bytes the child wrote. Whole reads, not lines: the parser is a state
    /// machine and an escape sequence split across two reads is ordinary.
    Output(Vec<u8>),
    /// The child is gone, which ends the session — a terminal outliving its
    /// shell is a window nobody can type into.
    Exit(ExitStatus),
    /// The child's output ending: its last slave is closed and nothing more
    /// will ever arrive on it.
    Drained,
    /// A producer stopping, and why. None of the three can return an error to
    /// anyone — nothing joins them — so each reports one and ends; the loop is
    /// where that becomes a failure the process exits on.
    Closed(String),
}

/// How many events may be in flight before a producer blocks. A blocked
/// producer is the point: the alternative to waiting is dropping events, and
/// a dropped configure is a window stuck at the wrong size — while a blocked
/// PTY reader is how §12 has the kernel's own buffer backpressure the child.
///
/// §10 bounds the output this process will hold for a child, and one channel
/// now carries that output along with everything else, so the QUEUE is what
/// has to honour the bound: its whole length in read chunks is the ceiling.
/// Wayland events share those slots rather than being given more, which costs
/// nothing that matters — they are small, and the loop drains them — while a
/// longer queue would let output alone exceed the bound.
const MAX_PENDING_EVENTS: usize = pty::MAX_OUTPUT_CHUNKS;

/// Checked where a test cannot be defeated by a filter, since all three sides
/// are constants: a queue that could hold more than §10's ceiling is a build
/// that does not happen. It would pass every test here otherwise — a queue
/// bounds memory, and nothing observes how much — while holding half again
/// as much of a child's output as the design says it may.
const _: () = assert!(MAX_PENDING_EVENTS * pty::READ_CHUNK <= pty::MAX_OUTPUT_BYTES);

/// The PTY reader's vocabulary in the loop's. Its ending is an event like any
/// other: output that stopped for a reason is a terminal that failed, and
/// output that simply ran out is a child whose last slave closed.
fn from_output(output: pty::Output) -> Event {
    match output {
        pty::Output::Bytes(bytes) => Event::Output(bytes),
        pty::Output::Ended(Ok(())) => Event::Drained,
        pty::Output::Ended(Err(error)) => Event::Closed(error),
    }
}

/// The child waiter's, the same way. A wait that failed leaves the terminal
/// unable to say how its child ended, which is a fault rather than an ending.
fn from_waited(waited: pty::Waited) -> Event {
    match waited {
        pty::Waited::Exited(status) => Event::Exit(status),
        pty::Waited::Failed(error) => Event::Closed(error),
    }
}

/// The child, from the loop's side: the queue everything typed or answered
/// goes into, and what the loop has learned about how it ended.
///
/// BOTH halves of the ending are
/// needed and they race: the kernel hangs the reader up and the waiter's
/// `wait` returns at the same instant, in either order.
///
/// Ending on the exit alone would drop whatever the child wrote last — the
/// bytes are still in the kernel, and `Event::Exit` can overtake them — and a
/// shell's parting line is exactly the case. Ending on the drain alone would
/// name no status, since the wait may not have returned yet. So the terminal
/// ends when its output has run out AND its child has been waited for, which
/// is deterministic in a way neither half is.
///
/// A child that closes its descriptors and keeps running therefore keeps the
/// terminal, and so does a grandchild still holding the slave after the child
/// is gone: in both cases one half has not happened, and in both cases there
/// is still something the terminal is for.
struct Child {
    input: Arc<pty::Input>,
    drained: bool,
    status: Option<ExitStatus>,
}

impl Default for Child {
    /// A child that is not there yet — startup, before `start`. Keys struck
    /// during presentation ARE pushed into this queue, and it is the queue
    /// `present` hands on rather than one that is dropped, so type-ahead
    /// survives into the writer that eventually drains it.
    fn default() -> Self {
        Child {
            input: pty::Input::new(),
            drained: false,
            status: None,
        }
    }
}

/// Read Wayland events until the connection ends, and hand each to the loop.
/// This thread never writes: every request goes out from the main loop, so
/// request order is one thread's property rather than something two have to
/// agree about.
fn spawn_wayland_reader(
    mut reader: conn::Reader,
    events: SyncSender<Event>,
) -> Result<JoinHandle<()>, String> {
    thread::Builder::new()
        .name("td-term-wayland".into())
        .spawn(move || loop {
            match reader.next() {
                Ok(message) => {
                    // The ONE descriptor the terminal expects — its keymap —
                    // is sent by td's server from `send_keyboard_initial`,
                    // once, as the keyboard is registered, and a second
                    // registration of that id is refused there. So it arrives
                    // and is claimed during `present`, before the reading
                    // half detaches. A compositor that re-sent one later
                    // would be closed here rather than verified: §11 puts
                    // other compositors outside this profile, and an
                    // unclaimed descriptor would otherwise be held open until
                    // the process ended.
                    let unclaimed = reader.pending_fd_count();
                    if unclaimed != 0 {
                        let _ = events.send(Event::Closed(format!(
                            "the terminal received {unclaimed} unexpected descriptor(s)"
                        )));
                        return;
                    }
                    if events.send(Event::Wayland(message)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    // Deliberately ignored: a closed receiver means the loop
                    // has already gone, which is the only reader of this.
                    let _ = events.send(Event::Closed(error));
                    return;
                }
            }
        })
        .map_err(|e| format!("spawn the Wayland reader thread: {e}"))
}

/// The compositor's published repeat timings, as a machine. `rate` is keys
/// per second and zero means "do not repeat at all", which the protocol
/// defines and which a division would otherwise turn into an interval of
/// infinity. A negative rate or delay is a compositor talking nonsense: both
/// are `int` on the wire and neither has a meaning below zero, so the pinned
/// default stands in rather than a saturating guess.
fn repeat_from(rate: i32, delay: i32) -> keys::Repeat {
    let (Ok(rate), Ok(delay)) = (u64::try_from(rate), u64::try_from(delay)) else {
        return keys::Repeat::new();
    };
    if rate == 0 {
        return keys::Repeat::disabled();
    }
    // Clamped as the interval is, and for a sharper reason: a delay of zero
    // arms a repeat that is due the instant it is pressed, and the loop dates
    // the arming and the serving from ONE `now` — so a single tap would send
    // its byte twice. Nothing td publishes trips it; a compositor that
    // published 0 is exactly what this conversion is defensive about.
    keys::Repeat::with_timing(delay.max(1), 1000 / rate)
}

/// Milliseconds since the terminal started. The repeat machine takes an
/// injected clock so its tests never sleep; this is the only place a real one
/// enters, and it is monotonic because `Instant` is.
fn now_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Hand the repeat machine the key `dispatch` read, now that there is a clock
/// to date it by. Separate from `serve_event` because a press is meaningless
/// without a time and `dispatch` deliberately has none.
fn arm_repeat(surface: &mut Surface, now: u64) {
    let Some((code, pressed)) = surface.pending_key.take() else {
        return;
    };
    if surface.group != 0 {
        // Arming here would repeat bytes `translate` refuses to send once.
        surface.repeat.cancel();
        return;
    }
    let (modifiers, modes) = (surface.modifiers, surface.modes);
    // The same `viewing` the other two call sites compute. `press` reads it
    // only to decide whether the key does anything at all, so today both
    // answers arm End alike — but a key that became silent on one side of it
    // would otherwise arm from a value no other site agrees with.
    let viewing = surface.viewport.viewing(surface.history);
    if pressed {
        surface.repeat.press(code, modifiers, modes, viewing, now);
    } else {
        surface.repeat.release(code);
    }
}

/// One repetition, if one is due. Routed exactly as a fresh press is —
/// through the same queue, ringing the same bell on refusal — because a
/// repeat the child cannot take is a repeat that should stop being sent
/// rather than one that silently vanishes.
fn serve_repeat(
    surface: &mut Surface,
    model: &mut Option<Terminal>,
    child: &mut Child,
    now: u64,
) -> Result<(), String> {
    // Read here rather than reused from the last keyboard event: a held
    // cursor key outlives the child's DECCKM changes, and rerouting per tick
    // is the whole reason `Repeat` stores a CODE rather than a sequence.
    let modes = keys::Modes {
        application_cursor: model
            .as_ref()
            .and_then(|terminal| terminal.mode("application-cursor"))
            .unwrap_or(false),
    };
    surface.history = model.as_ref().map(Terminal::scrollback).unwrap_or_default();
    let viewing = surface.viewport.viewing(surface.history);
    let action = match surface.repeat.due(now, modes, viewing) {
        Some(action) => action,
        None => return Ok(()),
    };
    let sequence = match action {
        keys::Action::Bytes(sequence) => sequence,
        // Holding Shift+PageUp is how a reader walks back through history,
        // so a repeated scroll moves the view exactly as the first one did.
        action @ keys::Action::Scroll(_) => {
            let rows = surface.cells.map_or(0, |(rows, _)| usize::from(rows));
            let before = surface.viewport.offset(surface.history);
            surface.viewport.apply(&action, rows, surface.history);
            // As above: a held PageUp at the top of history must not redraw
            // once per repetition.
            surface.stale |= surface.viewport.offset(surface.history) != before;
            return Ok(());
        }
        keys::Action::Silent => return Ok(()),
    };
    if !child.input.push(sequence.as_slice())? {
        if let Some(terminal) = model.as_mut() {
            terminal.ring();
            surface.stale = true;
        }
    }
    Ok(())
}

/// One turn of the terminal's steady state: answer an event.
/// A new size is adopted exactly as the first one was — same function, so
/// resize is not a second code path — and a configure that changes nothing
/// still gets the commit its acknowledgement owes.
///
/// It takes an event rather than reading one because after readiness the
/// reading happens on another thread; and it is a function rather than the
/// body of `run`'s loop because `run` does not return, so that loop is one no
/// test can enter.
fn serve_event(
    connection: &mut Connection,
    surface: &mut Surface,
    model: &mut Option<Terminal>,
    session: &Session,
    fallback: Size,
    child: &mut Child,
    event: Event,
) -> Result<(), String> {
    match event {
        Event::Closed(error) => return Err(error),
        Event::Exit(status) => child.status = Some(status),
        Event::Drained => child.drained = true,
        Event::Wayland(message) => {
            // Refreshed before rather than after: the mode in force when the
            // key was pressed is the one that spells it, and a child's reply
            // to an earlier key can change it. Only for the keyboard, though
            // — every frame callback and configure is a Wayland message too,
            // and none of them can consume a mode.
            if message.object == KEYBOARD {
                surface.modes = keys::Modes {
                    application_cursor: model
                        .as_ref()
                        .and_then(|terminal| terminal.mode("application-cursor"))
                        .unwrap_or(false),
                };
            }
            // The viewport clamps against this on every read, so it has to be
            // what history holds NOW rather than when the view was opened:
            // output and eviction both move it underneath. BOTH input devices
            // move that view — a wheel reading the cache a keystroke last left
            // would scroll against a history the child has since written to,
            // and after a boot with no keys pressed that is no history at all.
            if message.object == KEYBOARD || Some(message.object) == surface.pointer {
                surface.history = model.as_ref().map(Terminal::scrollback).unwrap_or_default();
            }
            if !connection.handle_common(&message)? {
                surface.dispatch(connection, &message, fallback)?;
            }
            if let Some(sequence) = surface.pending_input.take() {
                // Refused means the child is not draining, which §10 answers
                // with the bell rather than by growing the queue or dropping
                // the terminal. A key typed before the model exists cannot be
                // refused into anything, so there is nothing to ring.
                if !child.input.push(sequence.as_slice())? {
                    if let Some(terminal) = model.as_mut() {
                        terminal.ring();
                        // The ring is model state; without this nothing asks
                        // for the frame that would show it.
                        surface.stale = true;
                    }
                }
            }
        }
        Event::Output(bytes) => {
            let terminal = model
                .as_mut()
                .ok_or_else(|| "the child wrote before the terminal had a model".to_string())?;
            terminal.feed(&bytes);
            // Answered, now that there is a writer to answer through. ONE AT
            // A TIME, because §10's atomicity unit is a reply: pushing a
            // whole read's worth as one sequence would drop an answer that
            // fits because a later one did not.
            //
            // A refusal is the queue being full, which §10 defines as "that
            // sequence did not fit, ring the bell" — dropped WHOLE, since half
            // a `CSI` reaching the child is worse than no answer at all. A
            // writer that has DIED is the other outcome and is an error, not a
            // bell: a terminal beeping at every reply because its writer is
            // gone would be reporting the wrong news forever.
            let mut refused = false;
            for reply in terminal.take_replies() {
                refused |= !child.input.push(&reply)?;
            }
            if refused {
                terminal.ring();
            }
            surface.stale = true;
        }
    }
    // Both halves, in either order.
    if let (true, Some(status)) = (child.drained, child.status) {
        return Err(ended(status));
    }
    settle(connection, surface, model, session)
}

/// Put on screen whatever the turn just changed. Extracted because a
/// REPETITION changes the same things an event does — a held scroll moves the
/// view — and the loop's timeout branch has no event to carry it here. The
/// compositor suppresses evdev repeat, so a held key produces no Wayland
/// message at all: without this the view moved and the picture did not, until
/// the key was released.
fn settle(
    connection: &mut Connection,
    surface: &mut Surface,
    model: &mut Option<Terminal>,
    session: &Session,
) -> Result<(), String> {
    let Some(wanted) = surface.wanted() else {
        return Ok(());
    };
    // Throttled on the frame in flight, which is what stops a burst of
    // configures — or of output — becoming a burst of pools: only the LATEST
    // state is ever drawn, because what comes in between is superseded in the
    // surface and the model before anything allocates for it. The release and
    // the callback are events too, so the frame completing is what brings the
    // deferred redraw back here — there is nothing to wait on that is not
    // already an event.
    if !surface.frame_in_flight() {
        if surface.drawn != Some(wanted) {
            return adopt_size(connection, surface, model, session);
        }
        if surface.stale {
            return redraw(connection, surface, model, session);
        }
    }
    // The acknowledgement is still owed a commit even when the pixels cannot
    // be replaced yet.
    surface.commit_configure(connection)
}

/// Start the session's child on the PTY and put its output and its exit on
/// the loop's channel.
///
/// §12 orders this after the winsize is set and verified, which `present` has
/// already done: a shell that asks its terminal how big it is at startup — as
/// anything drawing a prompt does — must not be told a size the surface does
/// not have. The slave is consumed by the spawn and every parent-side clone
/// of it is dropped there, so the master is the only handle left and closing
/// it is the kernel's ordinary hangup.
fn start_child(
    pty: &Pty,
    events: &SyncSender<Event>,
    command: &pty::ChildCommand,
    account: &pty::Account,
    input: Arc<pty::Input>,
) -> Result<Started, String> {
    let output = pty
        .master()
        .try_clone()
        .map_err(|e| format!("duplicate the terminal device for its reader: {e}"))?;
    let sink = pty
        .master()
        .try_clone()
        .map_err(|e| format!("duplicate the terminal device for its writer: {e}"))?;
    let slave = pty.peer()?;
    let child = pty::spawn(
        command,
        &pty::environment(account),
        Path::new(&account.home),
        slave,
    )?;

    // Two producers, both feeding the one channel the loop serves. The reader
    // blocking on a full channel is how §12 has the kernel's PTY buffer
    // backpressure the child rather than this process buffering for it.
    //
    // The WAITER goes first, and that order is the child's safety rather than
    // a preference: from here the only fallible step left is a thread spawn,
    // and `spawn_waiter` takes the child back and reaps it when its own
    // fails. Spawning the reader first would drop a live `Child` on that
    // failure — no signal, no reap — leaving a process holding the slave, so
    // no hangup for anyone and a zombie once it does exit.
    let waiter = pty::spawn_waiter(child, events.clone(), from_waited)?;
    let reader = pty::spawn_reader(output, events.clone(), from_output)?;
    // The third thread, and the only one that does not produce events: it
    // consumes what the loop pushes. §12 puts it on its own thread because a
    // child in raw mode that stops reading blocks a write, and blocking the
    // loop in one would stop the terminal answering the compositor.
    let writer = pty::spawn_writer(sink, Arc::clone(&input))?;
    Ok((
        vec![waiter, reader, writer],
        Child {
            input,
            drained: false,
            status: None,
        },
    ))
}

/// The three threads a child is served by, and the loop's handle on it.
/// One value from one place: the queue the writer drains and the queue the
/// loop pushes to have to be the same object, and returning the pieces
/// separately is an invitation to wire up two.
type Started = (Vec<JoinHandle<Result<(), String>>>, Child);

/// What startup hands the steady state: the surface, the model, the grid the
/// kernel agreed to, and the input queue that may already hold type-ahead.
type Presented = (Surface, Terminal, (u16, u16), Arc<pty::Input>);

/// Those, plus the readiness socket for as long as the process is up.
type Running = (
    Vec<JoinHandle<Result<(), String>>>,
    Child,
    socket::Published,
);

/// Start the child, and only then advertise the terminal.
///
/// The ORDER is the contract: resolving the account, spawning the shell and
/// spawning its three threads can each fail, and a probe told the terminal is
/// up on a terminal whose shell never started has been told something that
/// was never true. It is a function rather than two lines of `run` because `run`
/// dials a socket and never returns, so an ordering asserted only there is one
/// no test can watch.
fn start(
    pty: &Pty,
    events: &SyncSender<Event>,
    status: &Path,
    passwd: &Path,
    ready_socket: &Path,
    cells: (u16, u16),
    input: Arc<pty::Input>,
) -> Result<Running, String> {
    let account = pty::current_account(status, passwd)?;
    let command = pty::child_command(Path::new(pty::CTTYHACK), &[])?;
    let (children, child) = start_child(pty, events, &command, &account, input)?;
    let (rows, columns) = cells;
    let published = ready::publish(ready_socket, rows, columns)?;
    Ok((children, child, published))
}

/// A child ending ends the session: a terminal outliving its shell is a
/// window nobody can type into, and td-svc restarting the service is what
/// puts a fresh one there. Reported rather than swallowed, because "the
/// shell exited" and "the compositor went away" are different things for
/// whoever reads the log.
fn ended(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("the terminal's child exited with status {code}"),
        None => format!("the terminal's child was killed by a signal ({status})"),
    }
}

/// The same turn, reading the event here. Startup has nothing else to serve,
/// so it reads on the main thread and the reader is not detached until the
/// terminal is up.
fn serve_turn(
    connection: &mut Connection,
    surface: &mut Surface,
    model: &mut Option<Terminal>,
    session: &Session,
    fallback: Size,
    child: &mut Child,
) -> Result<(), String> {
    let message = connection.next()?;
    // Startup has no child at all: the events that make an ending, and the
    // queue an answer would go into, both belong to threads `start` has not
    // spawned yet.
    serve_event(
        connection,
        surface,
        model,
        session,
        fallback,
        child,
        Event::Wayland(message),
    )
}

/// Everything `run` does before it may publish: bind, create the surface,
/// take the first configure, render one frame into it, and wait for BOTH the
/// buffer release and the frame callback. Production, so the test that drives
/// it against the real server drives what the binary runs.
fn present(
    connection: &mut Connection,
    directory: &Path,
    font: &Font,
    palette: &render::Palette,
    pty: &Pty,
) -> Result<Presented, String> {
    let session = Session {
        directory,
        pty,
        font,
        palette,
    };
    let fallback = default_size(font)?;
    let mut surface = Surface::new(bind_globals(connection)?);
    conn::create_surface(connection, TITLE)?;
    // Two frames, not one, and that is the protocol rather than a retry. The
    // compositor cannot tile a surface it has not mapped, so its first
    // configure is zero in both axes; presenting at the fallback is what maps
    // the surface, and the tile arrives in the configure that follows. §12
    // puts readiness after a frame, and this is the frame it means: one drawn
    // at a size the compositor CHOSE. Everything here runs under the
    // connection's deadline, so a compositor that never gets there fails.
    let mut model = None;
    // The SAME turn the steady state runs, so startup cannot throttle or
    // redraw differently from a resize — the only difference is that this
    // loop has somewhere to stop.
    let mut child = Child::default();
    while !surface.presented() {
        serve_turn(
            connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut child,
        )?;
    }
    let terminal = model.ok_or_else(|| "the terminal presented without a model".to_string())?;
    let cells = surface
        .cells
        .ok_or_else(|| "the terminal presented without a grid".to_string())?;

    let unclaimed = connection.pending_fd_count();
    if unclaimed != 0 {
        return Err(format!(
            "the terminal presentation retained {unclaimed} unexpected descriptors"
        ));
    }
    // The queue goes on rather than being dropped. A surface is focusable the
    // moment it maps, which is a frame BEFORE this loop ends, so a key struck
    // during startup is already translated and queued — and minting a second
    // queue for the writer would discard exactly the type-ahead a person
    // expects a terminal to have kept.
    Ok((surface, terminal, cells, child.input))
}

/// Everything that must hold before readiness may be published: a presented
/// frame at a size the compositor chose, and a PTY the kernel agrees is that
/// size. §12 is explicit that the readiness line names a grid the terminal
/// was actually SET to — a line describing a grid no terminal could have
/// taken is not readiness — so the grid returned here is the one read back
/// out of the kernel and not the one computed from the tile.
fn prepare(
    connection: &mut Connection,
    directory: &Path,
    font: &Font,
    palette: &render::Palette,
    ptmx: &Path,
) -> Result<Prepared, String> {
    // Before the first frame rather than after it: a machine whose devpts is
    // missing or misconfigured should fail without having drawn a window,
    // and nothing about the size is needed to open one.
    let pty = Pty::open(ptmx)?;
    let (surface, terminal, cells, input) = present(connection, directory, font, palette, &pty)?;
    Ok(Prepared {
        surface,
        pty,
        terminal,
        cells,
        input,
    })
}

/// The same sequence over an already-open stream, which is the only way to
/// drive it against the real server without a socket on disk.
#[cfg(test)]
pub fn prepare_for_test(
    stream: std::os::unix::net::UnixStream,
    directory: &Path,
    ptmx: &Path,
) -> Result<(Connection, Prepared), String> {
    let font = font::pinned()?;
    let palette = render::Palette::pinned();
    let deadline = Instant::now()
        .checked_add(HANDSHAKE_TIMEOUT)
        .ok_or_else(|| "could not bound the terminal's Wayland handshake".to_string())?;
    let mut connection = Connection::over(stream, Some(deadline), FIRST_DYNAMIC_ID);
    connection.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let prepared = prepare(&mut connection, directory, &font, &palette, ptmx)?;
    Ok((connection, prepared))
}

pub fn run(options: &Options) -> Result<(), String> {
    let runtime_directory = options
        .socket
        .parent()
        .ok_or_else(|| format!("Wayland socket {} has no parent", options.socket.display()))?;
    let font = font::pinned()?;
    let palette = render::Palette::pinned();
    let fallback = default_size(&font)?;
    let deadline = Instant::now()
        .checked_add(HANDSHAKE_TIMEOUT)
        .ok_or_else(|| "could not bound the terminal's Wayland handshake".to_string())?;
    let mut connection = Connection::connect(&options.socket, deadline, FIRST_DYNAMIC_ID)?;
    let Prepared {
        mut surface,
        pty,
        terminal,
        cells: (rows, columns),
        input,
    } = prepare(
        &mut connection,
        runtime_directory,
        &font,
        &palette,
        Path::new(pty::DEV_PTMX),
    )?;
    // The PTY lives as long as the loop below: the slave the child will be
    // given is allocated from this master, and every later configure resizes
    // it. The model lives that long for the stronger reason — it is the
    // screen, and rebuilding it on a resize would erase the session.
    let mut model = Some(terminal);
    let session = Session {
        directory: runtime_directory,
        pty: &pty,
        font: &font,
        palette: &palette,
    };
    connection.finish_handshake()?;

    // Everything that can still fail happens BEFORE readiness is advertised.
    // Both of these can — a descriptor limit, a thread limit — and a probe
    // that accepted the socket only to watch the terminal exit would have
    // been told something true for less than a second.
    let reader = connection.detach_reader()?;
    let (sender, events) = sync_channel(MAX_PENDING_EVENTS);
    let _wayland = spawn_wayland_reader(reader, sender.clone())?;
    let (_children, mut child, _ready) = start(
        &pty,
        &sender,
        Path::new(PROC_STATUS),
        Path::new(ETC_PASSWD),
        &options.ready_socket,
        (rows, columns),
        input,
    )?;
    // `write_all` rather than `print!`, which PANICS on a write failure — and
    // a panic here would abort past `Published::drop` and leave the socket
    // behind. One encoder, so the diagnostic an operator reads and the line a
    // probe parses cannot describe different grids.
    // `lock()` because `println!` held one for the whole write and this must
    // too: `write_all` on the unlocked handle re-takes it per `write` call, so
    // a marker could interleave with another thread's line and reach the
    // oracle as neither.
    let mut out = std::io::stdout().lock();
    out.write_all(ready::marker(rows, columns).as_bytes())
        .map_err(|e| format!("write terminal ready marker: {e}"))?;
    out.flush()
        .map_err(|e| format!("flush terminal ready marker: {e}"))?;

    // Three producers now — the Wayland reader, the PTY reader and the child
    // waiter — and one loop serving all of them, which is what §12 means by
    // "bounded messages to one main loop". Reading moved off the main thread
    // above, and not before: startup has one source and can afford to block
    // in it, while a terminal with a child cannot.
    //
    // The loop's own handle goes first, or `recv` can never fail: a channel
    // with a live sender in this thread stays open however few producers are
    // left, and a terminal every producer had abandoned would park here
    // forever holding a mapped surface. Each producer reports before it ends,
    // so this is the case where one did not.
    drop(sender);
    // The clock the repeat machine is dated by. Started here rather than at
    // process start so a slow handshake cannot make the first repetition
    // arrive early.
    let started = Instant::now();
    // Whatever presentation left unapplied. `present` serves events through
    // the same `dispatch`, so a key held across startup is recorded there and
    // has had no clock to be dated by until now; without this it would arm
    // only when some LATER message happened to arrive, or never.
    arm_repeat(&mut surface, now_ms(started));
    loop {
        // A held key is the only thing that makes this loop time-sensitive:
        // with none armed it blocks as it always did, and with one armed it
        // waits no longer than that key's next repetition. Waiting rather
        // than polling is what `Repeat::deadline` exists for.
        let event = match surface.repeat.deadline() {
            None => Some(
                events
                    .recv()
                    .map_err(|_| "every terminal producer stopped without reporting".to_string())?,
            ),
            Some(due) => {
                let wait = Duration::from_millis(due.saturating_sub(now_ms(started)));
                match events.recv_timeout(wait) {
                    Ok(event) => Some(event),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err("every terminal producer stopped without reporting".to_string())
                    }
                }
            }
        };
        match event {
            Some(event) => {
                serve_event(
                    &mut connection,
                    &mut surface,
                    &mut model,
                    &session,
                    fallback,
                    &mut child,
                    event,
                )?;
                let now = now_ms(started);
                arm_repeat(&mut surface, now);
                // A due repetition is served even though a message arrived:
                // `recv_timeout` hands back a QUEUED event before it reports
                // the expiry, so a child writing steadily would otherwise
                // starve the held key indefinitely.
                serve_repeat(&mut surface, &mut model, &mut child, now)?;
                settle(&mut connection, &mut surface, &mut model, &session)?;
            }
            // The wait expired, so a repetition is due rather than a message.
            None => {
                serve_repeat(&mut surface, &mut model, &mut child, now_ms(started))?;
                settle(&mut connection, &mut surface, &mut model, &session)?;
            }
        }
    }
}

pub fn selftest() -> Result<(), String> {
    let font = font::pinned()?;
    let fallback = default_size(&font)?;
    let (rows, columns) = grid(fallback, &font)?;
    let expected = (
        u16::try_from(DEFAULT_ROWS).map_err(|_| "default rows escape a grid".to_string())?,
        u16::try_from(DEFAULT_COLUMNS).map_err(|_| "default columns escape a grid".to_string())?,
    );
    if (rows, columns) != expected {
        return Err(format!(
            "the default surface holds {rows}x{columns} cells, not {DEFAULT_ROWS}x{DEFAULT_COLUMNS}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::{DISPLAY, KEYBOARD, POINTER, SEAT, SURFACE, SYNC_CALLBACK};
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// Distinct paths for tests that create one, since the suite runs them on
    /// threads of one process in a shared directory.
    static SEQ: AtomicU32 = AtomicU32::new(0);

    fn font() -> Font {
        font::pinned().unwrap()
    }

    /// The fallback is expressed in cells, so the grid it yields is exactly
    /// the grid it names — whatever the pinned font's cell size happens to be.
    #[test]
    fn the_default_surface_holds_exactly_the_default_grid() {
        let font = font();
        let size = default_size(&font).unwrap();
        assert_eq!(size.width, DEFAULT_COLUMNS * font.width());
        assert_eq!(size.height, DEFAULT_ROWS * font.height());
        assert_eq!(grid(size, &font).unwrap(), (24, 80));
    }

    /// A tile smaller than one cell is still a terminal, because the renderer
    /// clips; a zero-row grid is not, because `grid_size` refuses it.
    #[test]
    fn a_surface_too_small_for_a_cell_still_names_a_grid() {
        let font = font();
        let tiny = Size {
            width: 1,
            height: 1,
        };
        assert_eq!(grid(tiny, &font).unwrap(), (1, 1));
    }

    fn message(object: u32, opcode: u16, payload: Vec<u8>) -> wire::Message {
        wire::Message {
            object,
            opcode,
            payload,
        }
    }

    fn toplevel_configure(width: i32, height: i32) -> wire::Message {
        configure_with_states(width, height, &[])
    }

    /// The state array spelled out in NUMBERS rather than through the
    /// constant it is read with, so the constant is pinned by this rather
    /// than compared with itself.
    fn configure_with_states(width: i32, height: i32, states: &[u32]) -> wire::Message {
        let mut payload = Vec::new();
        payload.extend_from_slice(&width.to_ne_bytes());
        payload.extend_from_slice(&height.to_ne_bytes());
        let bytes = u32::try_from(states.len() * 4).unwrap();
        payload.extend_from_slice(&bytes.to_ne_bytes());
        for state in states {
            payload.extend_from_slice(&state.to_ne_bytes());
        }
        message(XDG_TOPLEVEL, 0, payload)
    }

    /// One request off the wire, as `(object, opcode, payload)`.
    fn said(peer: &mut UnixStream) -> (u32, u16, Vec<u8>) {
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut header = [0u8; 8];
        std::io::Read::read_exact(peer, &mut header).unwrap();
        let object = u32::from_ne_bytes([header[0], header[1], header[2], header[3]]);
        let opcode = u16::from_ne_bytes([header[4], header[5]]);
        let length = usize::from(u16::from_ne_bytes([header[6], header[7]]));
        let mut payload = vec![0u8; length.saturating_sub(8)];
        std::io::Read::read_exact(peer, &mut payload).unwrap();
        (object, opcode, payload)
    }

    fn surface_configure(serial: u32) -> wire::Message {
        message(XDG_SURFACE, 0, serial.to_ne_bytes().to_vec())
    }

    fn pair() -> (Connection, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let connection = Connection::over(ours, None, FIRST_DYNAMIC_ID);
        // Every test here writes the events it expects to be read. A change
        // that reads one more would otherwise block forever, and a gate has
        // nothing to interrupt it with — so an over-read is a failure with a
        // diagnostic rather than a hung run.
        connection
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        (connection, theirs)
    }

    /// Wayland ids must be allocated DENSELY, and td's own server only checks
    /// uniqueness — so nothing at runtime would report a gap. What the
    /// terminal uses has to be exactly 1..FIRST_DYNAMIC_ID with nothing
    /// skipped, and the demo's higher start would skip the pointer's id: the
    /// terminal's pointer is not a FIXED object, because it exists only where
    /// a seat offers the capability and an id reserved for one that is never
    /// created is precisely the gap this guards against.
    #[test]
    fn the_terminal_leaves_no_gap_before_its_first_dynamic_id() {
        let mut used = vec![
            DISPLAY,
            REGISTRY,
            SYNC_CALLBACK,
            COMPOSITOR,
            SHM,
            XDG_WM_BASE,
            SURFACE,
            XDG_SURFACE,
            XDG_TOPLEVEL,
            SEAT,
            KEYBOARD,
        ];
        used.sort_unstable();
        used.dedup();
        assert_eq!(
            used,
            (1..FIRST_DYNAMIC_ID).collect::<Vec<u32>>(),
            "the terminal's fixed ids are not dense up to its dynamic range"
        );
        // The demo reserves one for its pointer; this client must not, or a
        // keyboard-only seat leaves that id created by nobody. A const block,
        // since both sides are constants and a build that cannot satisfy it
        // should not produce a test binary.
        const { assert!(POINTER >= FIRST_DYNAMIC_ID) };
    }

    /// The assertion above is over CONSTANTS, so it cannot see the case that
    /// actually breaks: a seat offering no pointer. The terminal must still
    /// have allocated nothing below its dynamic range, and must still hand
    /// the next object the FIRST id in it — a reserved-and-unused id is the
    /// gap a compliant compositor refuses, and td's own server would not.
    #[test]
    fn a_keyboard_only_seat_leaves_no_id_reserved_and_uncreated() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        assert_eq!(connection.next_id_for_test(), FIRST_DYNAMIC_ID);
        let mut capabilities = Vec::new();
        capabilities.extend_from_slice(&SEAT_KEYBOARD.to_ne_bytes());
        surface
            .dispatch(&mut connection, &message(SEAT, 0, capabilities), fallback)
            .unwrap();
        assert!(surface.pointer.is_none());
        assert_eq!(
            connection.next_id_for_test(),
            FIRST_DYNAMIC_ID,
            "a keyboard-only seat consumed an id"
        );
        assert_eq!(connection.allocate_id().unwrap(), FIRST_DYNAMIC_ID);

        // And a seat that DOES offer one spends exactly that id on it, so the
        // pointer is inside the dense range rather than beside it.
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let mut capabilities = Vec::new();
        capabilities.extend_from_slice(&(SEAT_KEYBOARD | SEAT_POINTER).to_ne_bytes());
        surface
            .dispatch(&mut connection, &message(SEAT, 0, capabilities), fallback)
            .unwrap();
        assert_eq!(surface.pointer, Some(FIRST_DYNAMIC_ID));
        assert_eq!(
            connection.allocate_id().unwrap(),
            FIRST_DYNAMIC_ID.saturating_add(1)
        );
    }

    /// The size arrives on one event and takes effect on another. A toplevel
    /// configure alone must not move the surface, or a client would act on a
    /// size the compositor has not committed to.
    #[test]
    fn a_size_takes_effect_at_the_surface_configure_and_not_before() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            // The registry NAMES td's server assigns, not object ids: 3 is
            // wl_output there, which is why it is not one of these.
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 640,
            height: 480,
        };

        assert!(!surface
            .dispatch(&mut connection, &toplevel_configure(320, 200), fallback)
            .unwrap());
        assert_eq!(surface.current, None, "a proposal moved the surface");

        assert!(surface
            .dispatch(&mut connection, &surface_configure(7), fallback)
            .unwrap());
        assert_eq!(
            surface.current,
            Some(Size {
                width: 320,
                height: 200
            })
        );
    }

    /// A surface configure with no proposal before it is the compositor
    /// confirming what it already applied. Falling back here would resize a
    /// settled terminal to the default grid — silently, since a configure is
    /// not an error.
    #[test]
    fn a_bare_surface_configure_reuses_the_size_already_applied() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 640,
            height: 480,
        };
        let settled = Size {
            width: 1024,
            height: 768,
        };
        surface
            .dispatch(&mut connection, &toplevel_configure(1024, 768), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &surface_configure(1), fallback)
            .unwrap();
        assert_eq!(surface.current, Some(settled));

        // No toplevel configure this time: nothing was proposed at all.
        surface
            .dispatch(&mut connection, &surface_configure(2), fallback)
            .unwrap();
        assert_eq!(
            surface.current,
            Some(settled),
            "a bare configure resized a settled terminal"
        );
    }

    /// `wl_registry` has no destroy request, so a hotplug after discovery is
    /// delivered to a client that has stopped listening. Dying on it would
    /// take the terminal down when a monitor is plugged in.
    #[test]
    fn a_global_arriving_after_discovery_is_not_fatal() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        // A `global`: name, interface, version. Wayland strings carry their
        // length, a NUL, and padding to a four-byte boundary.
        let mut arrived = Vec::new();
        arrived.extend_from_slice(&9u32.to_ne_bytes());
        arrived.extend_from_slice(&10u32.to_ne_bytes());
        arrived.extend_from_slice(b"wl_output\0\0\0");
        arrived.extend_from_slice(&4u32.to_ne_bytes());
        // A `global_remove`: the name alone.
        let mut departed = Vec::new();
        departed.extend_from_slice(&9u32.to_ne_bytes());

        for (opcode, payload) in [(0u16, arrived), (1u16, departed)] {
            assert!(
                !surface
                    .dispatch(
                        &mut connection,
                        &message(REGISTRY, opcode, payload),
                        fallback
                    )
                    .unwrap(),
                "a registry event completed a configure"
            );
        }
        assert_eq!(surface.current, None, "a registry event moved the surface");

        // Tolerated is not unparsed: every other arm validates what it
        // consumes, and a truncated global is a broken compositor either way.
        assert!(surface
            .dispatch(&mut connection, &message(REGISTRY, 0, Vec::new()), fallback)
            .is_err());

        // A global this client BOUND going away is the case that is not
        // tolerable: every later request to that object is ignored by the
        // compositor, so the terminal would stall at its first buffer.
        for (name, interface) in [(1u32, "wl_compositor"), (2, "wl_shm"), (4, "xdg_wm_base")] {
            let refused = surface
                .dispatch(
                    &mut connection,
                    &message(REGISTRY, 1, name.to_ne_bytes().to_vec()),
                    fallback,
                )
                .unwrap_err();
            assert_eq!(
                refused,
                format!("compositor withdrew {interface} while it was in use")
            );
        }
    }

    /// Zero is the compositor declining to choose, per axis. A wholesale
    /// fallback would turn "keep your width, take this height" into a resize
    /// of both.
    #[test]
    fn each_axis_declines_independently() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 640,
            height: 480,
        };
        surface
            .dispatch(&mut connection, &toplevel_configure(0, 0), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &surface_configure(1), fallback)
            .unwrap();
        assert_eq!(
            surface.current,
            Some(fallback),
            "a configure declining both axes resized"
        );

        surface
            .dispatch(&mut connection, &toplevel_configure(0, 200), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &surface_configure(2), fallback)
            .unwrap();
        assert_eq!(
            surface.current,
            Some(Size {
                width: 640,
                height: 200
            }),
            "a declined axis did not keep the size it had"
        );
    }

    /// The acknowledgement is not optional: a compositor that never receives
    /// one stops configuring, and the terminal waits for a size it will not
    /// be sent again.
    #[test]
    fn a_surface_configure_is_acknowledged_with_its_own_serial() {
        let (mut connection, mut peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        surface
            .dispatch(&mut connection, &surface_configure(0x2a), fallback)
            .unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut said = [0u8; 12];
        std::io::Read::read_exact(&mut peer, &mut said).unwrap();
        let object = u32::from_ne_bytes([said[0], said[1], said[2], said[3]]);
        let opcode = u16::from_ne_bytes([said[4], said[5]]);
        let serial = u32::from_ne_bytes([said[8], said[9], said[10], said[11]]);
        assert_eq!((object, opcode, serial), (XDG_SURFACE, 4, 0x2a));
    }

    #[test]
    fn a_negative_or_malformed_configure_is_refused() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        // Both axes are checked, and the MESSAGE is what pins the check: a
        // negative size fails `usize::try_from` a few lines later whether the
        // guard is there or not, so only the diagnostic distinguishes a
        // compositor that proposed nonsense from an arithmetic surprise.
        for (width, height) in [(-1, 8), (8, -1)] {
            let refused = surface
                .dispatch(
                    &mut connection,
                    &toplevel_configure(width, height),
                    fallback,
                )
                .unwrap_err();
            assert_eq!(
                refused,
                format!("compositor configured a negative terminal size {width}x{height}")
            );
        }
        // A state array whose length is not a whole number of words.
        let mut payload = Vec::new();
        payload.extend_from_slice(&8i32.to_ne_bytes());
        payload.extend_from_slice(&8i32.to_ne_bytes());
        payload.extend_from_slice(&3u32.to_ne_bytes());
        assert!(surface
            .dispatch(
                &mut connection,
                &message(XDG_TOPLEVEL, 0, payload),
                fallback
            )
            .is_err());
    }

    #[test]
    fn a_close_request_and_an_unknown_event_are_both_refused() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        assert!(surface
            .dispatch(
                &mut connection,
                &message(XDG_TOPLEVEL, 1, Vec::new()),
                fallback
            )
            .is_err());
        assert!(surface
            .dispatch(&mut connection, &message(4242, 0, Vec::new()), fallback)
            .is_err());
    }

    /// The format is only NOTICED here, not acted on: the refusal belongs
    /// with the frame that would use it, as the demo's does in `commit_frame`.
    /// What this pins is that the advertisement is seen at all, since it
    /// arrives before the configure and would otherwise be easy to drop.
    #[test]
    fn the_pixel_format_is_noticed_when_it_is_advertised() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        assert!(!surface.xrgb);
        surface
            .dispatch(
                &mut connection,
                &message(SHM, 0, 9u32.to_ne_bytes().to_vec()),
                fallback,
            )
            .unwrap();
        assert!(!surface.xrgb, "an unrelated format was accepted");
        surface
            .dispatch(
                &mut connection,
                &message(SHM, 0, SHM_XRGB8888.to_ne_bytes().to_vec()),
                fallback,
            )
            .unwrap();
        assert!(surface.xrgb);
    }

    fn frame(released: bool, presented: bool) -> Frame {
        Frame {
            buffer: 20,
            callback: 21,
            released,
            presented,
        }
    }

    /// The release says the compositor is done READING the buffer; the
    /// callback says the frame reached the screen. Neither implies the other,
    /// and readiness means both.
    #[test]
    fn a_frame_is_presented_only_when_both_answers_have_come_back() {
        assert!(!frame(false, false).complete());
        assert!(!frame(true, false).complete(), "a release alone presented");
        assert!(!frame(false, true).complete(), "a callback alone presented");
        assert!(frame(true, true).complete());
    }

    /// A frame is not enough: the FIRST frame is drawn at the terminal's own
    /// fallback, because a compositor cannot tile a surface it has not mapped.
    /// Announcing then would announce a grid the compositor never chose.
    #[test]
    fn readiness_needs_a_chosen_size_as_well_as_a_presented_frame() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 640,
            height: 384,
        };

        // The compositor's opening configure: zero in both axes.
        surface
            .dispatch(&mut connection, &toplevel_configure(0, 0), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &surface_configure(1), fallback)
            .unwrap();
        surface.drawn = surface.wanted();
        surface.frame = Some(frame(true, true));
        assert!(
            !surface.ready(),
            "a frame at the fallback size counted as readiness"
        );

        // The configure that follows the surface being mapped chooses one.
        surface
            .dispatch(&mut connection, &toplevel_configure(592, 352), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &surface_configure(2), fallback)
            .unwrap();
        assert!(
            !surface.ready(),
            "the frame drawn at the OLD size counted as readiness for the new one"
        );

        // Only a frame drawn at the size now held is readiness.
        surface.drawn = surface.wanted();
        surface.frame = Some(frame(true, true));
        assert!(surface.ready());

        // The size is not the whole of what a frame answers. A configure that
        // only takes focus leaves the picture on screen wrong in the one way
        // a terminal shows focus at all, so it is not readiness either.
        surface
            .dispatch(
                &mut connection,
                &configure_with_states(592, 352, &[4]),
                fallback,
            )
            .unwrap();
        surface
            .dispatch(&mut connection, &surface_configure(3), fallback)
            .unwrap();
        assert!(
            !surface.ready(),
            "a frame drawn unfocused counted as readiness once focus arrived"
        );
        surface.drawn = surface.wanted();
        assert!(surface.ready());
    }

    /// The activation state decides whether the cursor is drawn as holding
    /// the keyboard, and NOTHING else observes it — so a wrong constant is a
    /// terminal that never looks focused, with every other test green. The
    /// state array here is written as the number 4, not as the constant the
    /// parser reads it with, or this would compare the constant with itself.
    ///
    /// It is also double-buffered with the size, because xdg-shell applies a
    /// whole configure at the surface event: the toplevel event alone must
    /// not move it.
    #[test]
    fn the_activation_state_is_read_by_value_and_applied_with_the_size() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 640,
            height: 384,
        };

        // MAXIMIZED, RESIZING and ACTIVATED — the state is found in a list
        // rather than being the only entry.
        surface
            .dispatch(
                &mut connection,
                &configure_with_states(592, 352, &[1, 3, 4]),
                fallback,
            )
            .unwrap();
        assert!(
            !surface.activated,
            "the toplevel configure applied the activation state on its own"
        );
        surface
            .dispatch(&mut connection, &surface_configure(1), fallback)
            .unwrap();
        assert!(
            surface.activated,
            "an ACTIVATED configure left the terminal unfocused"
        );

        // Every neighbouring state, and none of them is this one.
        surface
            .dispatch(
                &mut connection,
                &configure_with_states(592, 352, &[1, 2, 3, 5, 6]),
                fallback,
            )
            .unwrap();
        surface
            .dispatch(&mut connection, &surface_configure(2), fallback)
            .unwrap();
        assert!(
            !surface.activated,
            "a configure without ACTIVATED left the terminal focused"
        );
    }

    /// A compositor may choose one axis and decline the other, and that is
    /// still a compositor that chose. Requiring both would leave the terminal
    /// waiting for a configure it has already been sent, which presents as a
    /// twenty-second hang rather than as anything about layout.
    #[test]
    fn one_chosen_axis_is_enough_to_have_chosen() {
        let fallback = Size {
            width: 640,
            height: 384,
        };
        for (width, height) in [(592, 0), (0, 352), (592, 352)] {
            let (mut connection, _peer) = pair();
            let mut surface = Surface::new(Bound {
                compositor: 1,
                shm: 2,
                xdg_wm_base: 4,
                seat: 5,
            });
            surface
                .dispatch(
                    &mut connection,
                    &toplevel_configure(width, height),
                    fallback,
                )
                .unwrap();
            surface
                .dispatch(&mut connection, &surface_configure(1), fallback)
                .unwrap();
            assert!(
                surface.layout_configured,
                "a configure of {width}x{height} was not treated as a choice"
            );
        }

        // Zero in both axes is the compositor declining, which is what the
        // very first configure always is.
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface
            .dispatch(&mut connection, &toplevel_configure(0, 0), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &surface_configure(1), fallback)
            .unwrap();
        assert!(
            !surface.layout_configured,
            "the opening zero configure was treated as a choice"
        );
    }

    /// The pool a buffer came from is destroyed as soon as the buffer exists,
    /// so the buffer is the last handle on that mapping: a terminal that kept
    /// them would leak one mapping and one object id per frame, forever, with
    /// nothing failing until the compositor ran out.
    #[test]
    fn a_released_buffer_is_destroyed() {
        let (mut connection, mut peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        surface.live_buffers.insert(31);
        surface.frame = Some(Frame {
            buffer: 31,
            callback: 32,
            released: false,
            presented: false,
        });
        surface
            .dispatch(&mut connection, &message(31, 0, Vec::new()), fallback)
            .unwrap();
        assert_eq!(said(&mut peer), (31, 0, Vec::new()));
        assert!(!surface.live_buffers.contains(&31));
    }

    /// Acknowledging a configure is half of answering it: xdg-shell applies
    /// one at the surface commit that follows. A configure whose size is
    /// already drawn produces no frame and so no commit of its own — which is
    /// what a compositor choosing exactly the fallback looks like — so the
    /// bare commit is what stops that configure from being dropped.
    #[test]
    fn an_acknowledged_configure_is_applied_by_a_commit() {
        let (mut connection, mut peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        surface
            .dispatch(&mut connection, &surface_configure(7), fallback)
            .unwrap();
        assert_eq!(
            said(&mut peer),
            (XDG_SURFACE, 4, 7u32.to_ne_bytes().to_vec())
        );
        surface.commit_configure(&mut connection).unwrap();
        assert_eq!(said(&mut peer), (SURFACE, 6, Vec::new()));

        // One commit per configure, not one per turn of the loop: an empty
        // commit with nothing to apply is a frame the compositor may act on.
        surface.commit_configure(&mut connection).unwrap();
        surface
            .dispatch(&mut connection, &surface_configure(8), fallback)
            .unwrap();
        assert_eq!(
            said(&mut peer),
            (XDG_SURFACE, 4, 8u32.to_ne_bytes().to_vec())
        );
    }

    /// The bound is on the size a COMPOSITOR chose, so it has to hold before
    /// anything is allocated for that size — the terminal model included.
    /// An allocation too large to serve aborts, which no `Result` here could
    /// carry.
    #[test]
    fn a_frame_is_bounded_before_anything_is_allocated_for_it() {
        assert_eq!(
            frame_bytes(Size {
                width: 592,
                height: 352
            }),
            Ok(592 * 352 * render::BYTES_PER_PIXEL)
        );
        for (width, height) in [
            (0, 352),
            (592, 0),
            (MAX_UI_DIMENSION + 1, 352),
            (592, MAX_UI_DIMENSION + 1),
            (MAX_UI_DIMENSION, MAX_UI_DIMENSION),
            (usize::MAX, usize::MAX),
        ] {
            assert!(
                frame_bytes(Size { width, height }).is_err(),
                "{width}x{height} was accepted"
            );
        }
    }

    /// A configure after the first is the SAME event as the first, and the
    /// model is reflowed rather than replaced. Rebuilding it looks identical
    /// today — the terminal renders an empty model, so an erased screen and a
    /// preserved one are the same pixels — which is why this writes to the
    /// model first. That is the only way to tell the two apart before a child
    /// lands, and by then the difference is a session's scrollback.
    ///
    /// It also pins the ORDER §12 fixes: the PTY holds the new grid, so a
    /// child told to redraw is told about the size the surface now has.
    #[test]
    fn a_later_size_reflows_the_model_and_the_pty_rather_than_replacing_them() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.xrgb = true;
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let mut model = None;

        let first = default_size(&font).unwrap();
        surface.current = Some(first);
        adopt_size(&mut connection, &mut surface, &mut model, &session).unwrap();
        assert_eq!(surface.cells, Some((24, 80)));
        let window = pty.window().unwrap();
        assert_eq!((window.rows, window.columns), (24, 80));

        model.as_mut().unwrap().feed(b"reflow me");
        assert!(model
            .as_ref()
            .unwrap()
            .row_text(0)
            .unwrap()
            .starts_with("reflow me"));

        let second = Size {
            width: 592,
            height: 352,
        };
        surface.current = Some(second);
        adopt_size(&mut connection, &mut surface, &mut model, &session).unwrap();
        assert_eq!(surface.cells, Some((22, 74)));
        let window = pty.window().unwrap();
        assert_eq!(
            (window.rows, window.columns),
            (22, 74),
            "the PTY kept the size the surface no longer has"
        );
        let model = model.as_ref().unwrap();
        assert_eq!((model.rows(), model.columns()), (22, 74));
        assert!(
            model.row_text(0).unwrap().starts_with("reflow me"),
            "the resize replaced the terminal instead of reflowing it"
        );
        assert_eq!(
            surface.drawn,
            Some(Drawn {
                size: second,
                activated: false
            })
        );
    }

    /// A configure that asks for nothing new still has to be APPLIED, and
    /// applying one is a commit. `Surface::commit_configure` has its own test;
    /// what this pins is that the loop reaches it, which is the half DESIGN.md
    /// §12 spells out and the half a `serve_turn` that only ever drew would
    /// silently drop. The surface is primed by hand rather than by drawing, so
    /// the only two requests on the wire are the ones being asserted.
    #[test]
    fn the_steady_state_loop_applies_a_configure_it_need_not_draw() {
        let (mut connection, mut peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        surface.current = Some(fallback);
        surface.drawn = Some(Drawn {
            size: fallback,
            activated: false,
        });
        surface.frame = Some(frame(true, true));
        let mut model = None;

        write_event(&mut peer, &surface_configure(12));
        serve_turn(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut Child::default(),
        )
        .unwrap();

        assert_eq!(
            said(&mut peer),
            (XDG_SURFACE, 4, 12u32.to_ne_bytes().to_vec())
        );
        assert_eq!(
            said(&mut peer),
            (SURFACE, 6, Vec::new()),
            "the loop acknowledged a configure and never applied it"
        );
        assert!(!surface.needs_commit);
    }

    /// A configure the terminal cannot serve ends the session rather than
    /// being ignored or clamped. The bound is shared with the server's own
    /// per-client budget, so a tile refused here is one the server would
    /// refuse too — showing the old size forever, or a size nobody asked
    /// for, would both be lying about a window someone is looking at.
    #[test]
    fn a_configure_too_large_to_serve_ends_the_loop() {
        let (mut connection, mut peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.xrgb = true;
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        let mut model = None;

        write_event(&mut peer, &toplevel_configure(16_000, 16_000));
        write_event(&mut peer, &surface_configure(13));
        let mut refused = None;
        for _ in 0..2 {
            if let Err(error) = serve_turn(
                &mut connection,
                &mut surface,
                &mut model,
                &session,
                fallback,
                &mut Child::default(),
            ) {
                refused = Some(error);
            }
        }
        let refused = refused.expect("an unservable configure was accepted");
        assert!(
            refused.contains(&MAX_UI_FRAME_BYTES.to_string()),
            "{refused}"
        );
    }

    /// What an event IS, for a panic message that has to name the one it did
    /// not want without matching every arm at each call site.
    fn screen(terminal: &Terminal) -> String {
        (0..terminal.rows())
            .map(|row| terminal.row_text(row).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn named(event: &Event) -> &'static str {
        match event {
            Event::Wayland(_) => "a Wayland event",
            Event::Output(_) => "child output",
            Event::Exit(_) => "a child exit",
            Event::Drained => "the end of the child's output",
            Event::Closed(_) => "a closed connection",
        }
    }

    /// The wire form of an event, so a test can hand the client one the way a
    /// compositor would rather than calling `dispatch` behind the loop's back.
    fn write_event(peer: &mut UnixStream, message: &wire::Message) {
        let size = u32::try_from(message.payload.len() + 8).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&message.object.to_ne_bytes());
        bytes.extend_from_slice(&((size << 16) | u32::from(message.opcode)).to_ne_bytes());
        bytes.extend_from_slice(&message.payload);
        std::io::Write::write_all(peer, &bytes).unwrap();
    }

    /// The steady-state loop serves a resize — and serves only the LAST one.
    ///
    /// Two things are pinned here and neither is `adopt_size`'s own business.
    /// The first is that the loop `run` never leaves reaches a redraw at all:
    /// a loop that only acknowledged configures would strand the terminal at
    /// its startup grid, and nothing would look wrong, because a compositor
    /// showing a stale buffer scales nothing and reports nothing.
    ///
    /// The second is the throttle. A configure arriving while a frame is out
    /// must NOT draw: every draw is a fresh wl_shm pool, and a burst of legal
    /// configures would stack them until the compositor's aggregate limit
    /// disconnected the terminal. So the intermediate size here is superseded
    /// before anything allocates for it, and the count of live buffers says
    /// so — it stays at one across both configures, and the size that
    /// eventually lands is the second one.
    #[test]
    fn the_steady_state_loop_adopts_the_last_size_and_only_once_the_frame_is_back() {
        let (mut connection, mut peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.xrgb = true;
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        let mut model = None;
        surface.current = Some(fallback);
        adopt_size(&mut connection, &mut surface, &mut model, &session).unwrap();
        let first = surface.frame.as_ref().unwrap();
        let (buffer, callback) = (first.buffer, first.callback);

        // Two configures, back to back, with the first frame still out.
        write_event(&mut peer, &toplevel_configure(704, 400));
        write_event(&mut peer, &surface_configure(9));
        write_event(&mut peer, &toplevel_configure(592, 352));
        write_event(&mut peer, &surface_configure(10));
        for _ in 0..4 {
            serve_turn(
                &mut connection,
                &mut surface,
                &mut model,
                &session,
                fallback,
                &mut Child::default(),
            )
            .unwrap();
        }
        assert_eq!(
            surface.drawn,
            Some(Drawn {
                size: fallback,
                activated: false
            }),
            "a configure was drawn while a frame was still out"
        );
        assert_eq!(
            surface.live_buffers.len(),
            1,
            "each deferred configure allocated a buffer of its own"
        );

        // The frame comes back, and the redraw it was holding up happens on
        // that event rather than needing one of its own.
        write_event(&mut peer, &message(buffer, 0, Vec::new()));
        write_event(
            &mut peer,
            &message(callback, 0, 1u32.to_ne_bytes().to_vec()),
        );
        for _ in 0..2 {
            serve_turn(
                &mut connection,
                &mut surface,
                &mut model,
                &session,
                fallback,
                &mut Child::default(),
            )
            .unwrap();
        }

        let second = Size {
            width: 592,
            height: 352,
        };
        assert_eq!(
            surface.drawn,
            Some(Drawn {
                size: second,
                activated: false
            }),
            "the size the terminal settled on was not the last one configured"
        );
        assert_eq!(surface.live_buffers.len(), 1);
        let window = pty.window().unwrap();
        assert_eq!((window.rows, window.columns), (22, 74));
        let model = model.as_ref().unwrap();
        assert_eq!((model.rows(), model.columns()), (22, 74));
    }

    /// A configure that changes only the focus still needs a new picture: the
    /// cursor is drawn inverted when the terminal holds the keyboard and
    /// hollow when it does not, and nothing else on screen says which. A
    /// redraw decided on the size alone would leave a window looking focused
    /// after it stopped being.
    #[test]
    fn losing_or_taking_focus_redraws_at_the_same_size() {
        let (mut connection, mut peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.xrgb = true;
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        let mut model = None;
        surface.current = Some(fallback);
        adopt_size(&mut connection, &mut surface, &mut model, &session).unwrap();
        let first = surface.frame.as_ref().unwrap();
        let (buffer, callback) = (first.buffer, first.callback);
        write_event(&mut peer, &message(buffer, 0, Vec::new()));
        write_event(
            &mut peer,
            &message(callback, 0, 1u32.to_ne_bytes().to_vec()),
        );

        // Same size, ACTIVATED — the one thing that changed is the focus.
        let (width, height) = (
            i32::try_from(fallback.width).unwrap(),
            i32::try_from(fallback.height).unwrap(),
        );
        write_event(&mut peer, &configure_with_states(width, height, &[4]));
        write_event(&mut peer, &surface_configure(11));
        for _ in 0..4 {
            serve_turn(
                &mut connection,
                &mut surface,
                &mut model,
                &session,
                fallback,
                &mut Child::default(),
            )
            .unwrap();
        }
        assert_eq!(
            surface.drawn,
            Some(Drawn {
                size: fallback,
                activated: true
            }),
            "taking focus at the same size did not redraw"
        );
        assert_ne!(
            surface.frame.as_ref().unwrap().buffer,
            buffer,
            "the focus change reused the frame drawn without it"
        );
    }

    /// The terminal asks for no descriptors, so one arriving is an event it
    /// has misread — and an unclaimed descriptor is held open for as long as
    /// the session runs. `present` makes this check of the handshake; the
    /// reader thread makes it of everything after, which is the part nothing
    /// else looks at.
    #[test]
    fn the_reader_thread_refuses_a_descriptor_it_never_asked_for() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut connection = Connection::over(ours, None, FIRST_DYNAMIC_ID);
        let (held, _watcher) = UnixStream::pair().unwrap();
        connection.queue_fd_for_test(std::os::fd::IntoRawFd::into_raw_fd(held));
        write_event(&mut theirs, &surface_configure(5));
        let reader = connection.detach_reader().unwrap();
        let (sender, events) = sync_channel(MAX_PENDING_EVENTS);
        let thread = spawn_wayland_reader(reader, sender).unwrap();

        match events.recv_timeout(Duration::from_secs(10)).unwrap() {
            Event::Closed(error) => assert!(error.contains("unexpected descriptor"), "{error}"),
            other => panic!("an unclaimed descriptor was served as {}", named(&other)),
        }
        thread.join().unwrap();
    }

    /// The reader thread is where every event comes from once the terminal is
    /// up, so what it does at the END of the connection is the whole of how
    /// the loop learns anything is wrong: it cannot return an error to
    /// anybody, and a loop still waiting on a channel nobody will send to is
    /// a terminal that hangs instead of exiting.
    #[test]
    fn the_reader_thread_reports_the_connection_ending() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut connection = Connection::over(ours, None, FIRST_DYNAMIC_ID);
        write_event(&mut theirs, &surface_configure(4));
        let reader = connection.detach_reader().unwrap();
        let (sender, events) = sync_channel(MAX_PENDING_EVENTS);
        let thread = spawn_wayland_reader(reader, sender).unwrap();

        match events.recv_timeout(Duration::from_secs(10)).unwrap() {
            Event::Wayland(message) => {
                assert_eq!((message.object, message.opcode), (XDG_SURFACE, 0));
            }
            other => panic!("the reader produced {} instead of an event", named(&other)),
        }
        drop(theirs);
        match events.recv_timeout(Duration::from_secs(10)).unwrap() {
            Event::Closed(error) => assert!(error.contains("closed the connection"), "{error}"),
            other => panic!("a closed connection produced {}", named(&other)),
        }
        thread.join().unwrap();
    }

    /// The loop treats the reader stopping as its own failure. `run` never
    /// returns otherwise, so this is the path by which a compositor going
    /// away ends the terminal rather than parking it forever.
    #[test]
    fn a_closed_connection_ends_the_loop() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        let mut model = None;
        let refused = serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut Child::default(),
            Event::Closed("compositor went away".into()),
        )
        .err()
        .unwrap();
        assert_eq!(refused, "compositor went away");
    }

    /// Output feeds the model even when the picture cannot be replaced yet,
    /// and the redraw it asks for waits for the frame in flight. Dropping the
    /// bytes instead would lose whatever a child wrote between two frames.
    #[test]
    fn output_while_a_frame_is_in_flight_feeds_the_model_and_waits() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.xrgb = true;
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        surface.current = Some(fallback);
        let mut model = None;
        adopt_size(&mut connection, &mut surface, &mut model, &session).unwrap();
        let drawn = surface.drawn;
        surface.frame = Some(frame(false, false));

        serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut Child::default(),
            Event::Output(b"held".to_vec()),
        )
        .unwrap();
        assert!(screen(model.as_ref().unwrap()).contains("held"));
        assert!(surface.stale, "the deferred redraw was forgotten");
        assert_eq!(surface.drawn, drawn, "a frame was drawn over one in flight");

        // The frame comes back, and the next turn is what spends the debt.
        surface.frame = Some(frame(true, true));
        serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut Child::default(),
            Event::Output(b"more".to_vec()),
        )
        .unwrap();
        assert!(!surface.stale);
        assert!(surface.frame_in_flight(), "the redraw produced no frame");
    }

    /// A wheel reads the SCROLLBACK CACHE, and only a keyboard event used to
    /// refresh it. Driven through `serve_event` rather than by assigning the
    /// cache, because assigning it is exactly what hides this: after a boot
    /// with no key pressed the cache is empty, so the child could print a
    /// screenful and the wheel would still see nothing to scroll. ("See"
    /// rather than the obvious verb: this file is `include_str!`'d into the
    /// td-compositor recipe, so its text is scanned as a bootstrap step's and
    /// that verb is a retired host command.)
    #[test]
    fn a_wheel_scrolls_output_that_arrived_without_a_keystroke() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.xrgb = true;
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        surface.current = Some(fallback);
        let mut model = None;
        adopt_size(&mut connection, &mut surface, &mut model, &session).unwrap();

        let mut serve = |surface: &mut Surface, model: &mut Option<Terminal>, event| {
            serve_event(
                &mut connection,
                surface,
                model,
                &session,
                fallback,
                &mut Child::default(),
                event,
            )
            .unwrap()
        };

        // The seat offers a pointer. No keystroke anywhere in this test.
        let mut capabilities = Vec::new();
        capabilities.extend_from_slice(&(SEAT_KEYBOARD | SEAT_POINTER).to_ne_bytes());
        serve(
            &mut surface,
            &mut model,
            Event::Wayland(message(SEAT, 0, capabilities)),
        );
        let pointer = surface.pointer.expect("the seat offered a pointer");

        // Enough output to push lines into history.
        let mut written = Vec::new();
        for line in 0..60u32 {
            written.extend_from_slice(format!("L{line}\r\n").as_bytes());
        }
        serve(&mut surface, &mut model, Event::Output(written));
        let held = model.as_ref().map(Terminal::scrollback).unwrap_or_default();
        assert!(held.lines > 0, "the fixture pushed nothing into history");

        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_ne_bytes());
        payload.extend_from_slice(&(-1i32).to_ne_bytes());
        serve(
            &mut surface,
            &mut model,
            Event::Wayland(message(pointer, 8, payload)),
        );
        serve(
            &mut surface,
            &mut model,
            Event::Wayland(message(pointer, 5, Vec::new())),
        );
        assert_eq!(
            surface.viewport.offset(held),
            usize::try_from(LINES_PER_NOTCH).unwrap(),
            "the wheel scrolled against a stale history"
        );
    }

    /// Output before the first configure has nothing to feed. It cannot happen
    /// — the child is started after the terminal is up — so it is a fault
    /// rather than something to swallow.
    #[test]
    fn output_before_a_model_exists_is_a_failure() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        let mut model = None;
        let error = serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut Child::default(),
            Event::Output(b"early".to_vec()),
        )
        .err()
        .unwrap();
        assert!(error.contains("before the terminal had a model"), "{error}");
    }

    /// A terminal that could not start its shell is not a terminal, and a
    /// probe must not be able to accept one. The account file is the failure
    /// injected because it is the first thing `start_child` reads; what is
    /// asserted is not the error but the SOCKET, which is what a probe sees.
    ///
    /// This holds under BOTH orderings — publishing first and failing after
    /// leaves nothing behind either, because `socket::Published` unlinks on
    /// drop — so it is the outcome that is pinned here and not the order.
    /// What the order buys beyond it is the WINDOW: published first, the
    /// socket is real for as long as the spawn takes, and a probe that
    /// connected inside it would be answered.
    #[test]
    fn a_terminal_that_cannot_start_its_child_is_never_advertised() {
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let (sender, _events) = sync_channel(MAX_PENDING_EVENTS);
        let directory = std::env::temp_dir();
        let ready_socket = directory.join(format!(
            "td-term-unstarted-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let missing = directory.join("td-term-no-such-passwd");
        assert!(!missing.exists(), "the fixture path exists on this host");

        let error = start(
            &pty,
            &sender,
            Path::new(PROC_STATUS),
            &missing,
            &ready_socket,
            (22, 74),
            pty::Input::new(),
        )
        .err()
        .unwrap();
        assert!(error.contains("td-term-no-such-passwd"), "{error}");
        assert!(
            !ready_socket.exists(),
            "readiness was published for a terminal with no child"
        );
    }

    /// The BELL is deliberately kept where a reply is consumed: §10 says the
    /// next submitted frame inverts a one-pixel ring and clears the bit after
    /// release, and taking it here would destroy the only record that a
    /// notification ever happened. The renderer that presents it is a later
    /// landing; the bit has to survive until then.
    #[test]
    fn a_bell_waits_to_be_drawn_rather_than_being_taken_here() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.xrgb = true;
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        surface.current = Some(fallback);
        let mut model = None;
        adopt_size(&mut connection, &mut surface, &mut model, &session).unwrap();
        surface.frame = Some(frame(true, true));

        serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut Child::default(),
            Event::Output(b"\x07".to_vec()),
        )
        .unwrap();
        assert!(
            model.as_mut().unwrap().take_bell(),
            "the bell was consumed before anything could present it"
        );
    }

    /// The answer to a query the child asked reaches the child. This is the
    /// whole return path in one test: the model composes a reply, the loop
    /// hands it to the input queue, the writer thread takes it off, and the
    /// kernel puts it on the other side of the terminal.
    ///
    /// The trailing newline is the TEST's own, standing in for the keystroke
    /// that has not landed yet: the slave is in the kernel's canonical mode,
    /// where a read returns nothing until a line is complete, and a cursor
    /// report carries no newline of its own. A real child asking this question
    /// has put its terminal in raw mode first.
    #[test]
    fn a_query_the_child_asked_is_answered_through_the_writer() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.xrgb = true;
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let mut slave = pty.peer().unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        surface.current = Some(fallback);
        let mut model = None;
        adopt_size(&mut connection, &mut surface, &mut model, &session).unwrap();
        surface.frame = Some(frame(true, true));

        let input = pty::Input::new();
        let writer = pty::spawn_writer(pty.master().try_clone().unwrap(), Arc::clone(&input))
            .expect("the writer thread never started");
        let mut child = Child {
            input: Arc::clone(&input),
            drained: false,
            status: None,
        };
        serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut child,
            Event::Output(b"\x1b[6n".to_vec()),
        )
        .unwrap();
        assert!(input.push(b"\n").unwrap(), "the queue refused the newline");

        // Read on a thread, and wait with a deadline: `cargo test` has no
        // per-test timeout, so a writer that never wrote would hang the whole
        // binary with no diagnostic rather than failing here.
        let (chunks, arrived) = sync_channel(16);
        std::thread::spawn(move || loop {
            let mut chunk = [0u8; 64];
            let Ok(count) = slave.read(&mut chunk) else {
                return;
            };
            if count == 0
                || chunks
                    .send(chunk.get(..count).unwrap_or_default().to_vec())
                    .is_err()
            {
                return;
            }
        });
        let mut seen = Vec::new();
        while !seen.contains(&b'\n') {
            let chunk = arrived
                .recv_timeout(Duration::from_secs(30))
                .expect("the terminal never answered");
            seen.extend_from_slice(&chunk);
        }
        assert_eq!(
            seen, b"\x1b[1;1R\n",
            "the child was told something other than where the cursor is, exactly once"
        );
        assert!(
            !model.as_mut().unwrap().take_bell(),
            "an answer that was delivered rang the bell anyway"
        );
        input.close().unwrap();
        writer.join().unwrap().unwrap();
    }

    /// One reply fitting and the next not is the case a batched push gets
    /// wrong: §10's atomicity unit is a REPLY, so an answer that fits must go
    /// even when a later one cannot. Pushing a whole read's worth as one
    /// sequence drops both, and the child that asked the first question waits
    /// for an answer the terminal had room to send.
    #[test]
    fn a_reply_that_fits_goes_even_when_the_next_one_does_not() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.xrgb = true;
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        surface.current = Some(fallback);
        let mut model = None;
        adopt_size(&mut connection, &mut surface, &mut model, &session).unwrap();
        surface.frame = Some(frame(true, true));

        // Room for exactly one cursor report and not two. Nothing drains this
        // queue, which is the state a child that stopped reading leaves it in.
        let mut child = Child::default();
        let report = b"\x1b[1;1R".len();
        let filler = crate::keys::MAX_INPUT_BYTES - report;
        assert!(child.input.push(&vec![b'x'; filler]).unwrap());

        serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut child,
            Event::Output(b"\x1b[6n\x1b[6n".to_vec()),
        )
        .unwrap();
        // The second was refused, so the bell rings — and the first is IN, so
        // the queue has no room left at all.
        assert!(
            model.as_mut().unwrap().take_bell(),
            "the refused reply rang no bell"
        );
        assert!(
            !child.input.push(b"z").unwrap(),
            "the reply that fitted was dropped with the one that did not"
        );
    }

    /// PARTIAL room is what "dropped whole" is about, and it is the case a
    /// full queue cannot show: with no room at all, a byte-at-a-time push
    /// admits nothing either. Here there is room for SOME of the reply, and
    /// what must not happen is the child finding half a `CSI` in its input.
    #[test]
    fn a_reply_with_room_for_only_part_of_it_leaves_none_of_it() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.xrgb = true;
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        surface.current = Some(fallback);
        let mut model = None;
        adopt_size(&mut connection, &mut surface, &mut model, &session).unwrap();
        surface.frame = Some(frame(true, true));

        // Three bytes free against a six-byte cursor report.
        let spare = 3;
        let mut child = Child::default();
        assert!(child
            .input
            .push(&vec![b'x'; crate::keys::MAX_INPUT_BYTES - spare])
            .unwrap());

        serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut child,
            Event::Output(b"\x1b[6n".to_vec()),
        )
        .unwrap();
        assert!(
            model.as_mut().unwrap().take_bell(),
            "the dropped reply rang no bell"
        );
        // The room is still there, byte for byte: anything admitted would
        // have eaten into it, and a prefix of a `CSI` is what the child would
        // then read.
        assert!(
            child.input.push(&vec![b'y'; spare]).unwrap(),
            "part of the refused reply was admitted"
        );
        assert!(!child.input.push(b"z").unwrap(), "the queue was not full");
    }

    /// The refusal that a `refused = ` rather than a `refused |= ` loses: the
    /// FIRST reply of a batch dropped and a later one admitted. Replies differ
    /// in length, so this is reachable rather than theoretical — and a bell
    /// that only reports the last reply of a read is one §10 does not
    /// describe.
    #[test]
    fn a_refusal_rings_the_bell_even_when_a_later_reply_fits() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.xrgb = true;
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        surface.current = Some(fallback);
        let mut model = None;
        adopt_size(&mut connection, &mut surface, &mut model, &session).unwrap();
        surface.frame = Some(frame(true, true));

        // Room for the SHORTER second reply and not the longer first: the
        // device attributes answer is seven bytes and the status report four.
        let mut child = Child::default();
        let attributes = b"\x1b[?1;0c".len();
        let status = b"\x1b[0n".len();
        assert!(
            attributes > status,
            "the fixture no longer distinguishes them"
        );
        let filler = crate::keys::MAX_INPUT_BYTES - status;
        assert!(child.input.push(&vec![b'x'; filler]).unwrap());

        serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut child,
            Event::Output(b"\x1b[c\x1b[5n".to_vec()),
        )
        .unwrap();
        assert!(
            model.as_mut().unwrap().take_bell(),
            "the reply that was dropped rang no bell"
        );
        // The short one went, so nothing is left.
        assert!(
            !child.input.push(b"z").unwrap(),
            "the reply that fitted was dropped with the one that did not"
        );
    }

    /// A writer that has DIED is an error and ends the session; it is NOT the
    /// bell a full queue rings. §12 turns on that distinction and nothing at
    /// the loop pinned it — `Input`'s own tests cover the queue reporting a
    /// death, and this covers the loop acting on it.
    #[test]
    fn a_dead_writer_ends_the_session_rather_than_ringing() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.xrgb = true;
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        surface.current = Some(fallback);
        let mut model = None;
        adopt_size(&mut connection, &mut surface, &mut model, &session).unwrap();
        surface.frame = Some(frame(true, true));

        // A sink whose every write fails, so the writer dies on its first and
        // the death is not a race. Deliberately NOT the PTY master: writing
        // to one whose slave is unopened, or opened and closed, does not
        // reliably fail here — the writer parks in `Condvar::wait` and the
        // join below hangs rather than failing, which is a test that never
        // reports. `/dev/full` is the kernel's own always-fails device, as
        // `/dev/ptmx` beside it is its terminal multiplexer.
        let mut child = Child::default();
        let sink = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .expect("the gate has no /dev/full to fail a write against");
        let writer = pty::spawn_writer(sink, Arc::clone(&child.input)).unwrap();
        assert!(child.input.push(b"doomed").unwrap());
        writer.join().unwrap().unwrap_err();

        let failed = serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut child,
            Event::Output(b"\x1b[6n".to_vec()),
        )
        .err()
        .unwrap();
        assert!(
            failed.contains("stopped accepting input"),
            "a dead writer read as a full queue: {failed}"
        );
        assert!(
            !model.as_mut().unwrap().take_bell(),
            "a dead writer rang the bell instead of ending the session"
        );
    }

    /// A queue with NO room rings the bell. The "dropped whole" half of §10 is
    /// not what this shows — with nothing free, admitting byte by byte admits
    /// nothing either — and the test above is where that lives.
    #[test]
    fn a_reply_with_no_room_at_all_rings_the_bell() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.xrgb = true;
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        surface.current = Some(fallback);
        let mut model = None;
        adopt_size(&mut connection, &mut surface, &mut model, &session).unwrap();
        surface.frame = Some(frame(true, true));

        // Nothing drains this queue: there is no writer, which is the state a
        // child that has stopped reading puts a live one in.
        let mut child = Child::default();
        assert!(child
            .input
            .push(&vec![b'x'; crate::keys::MAX_INPUT_BYTES])
            .unwrap());
        assert!(
            !child.input.push(b"y").unwrap(),
            "the queue admitted more than its own bound"
        );

        serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut child,
            Event::Output(b"\x1b[6n".to_vec()),
        )
        .unwrap();
        assert!(
            model.as_mut().unwrap().take_bell(),
            "a dropped reply rang no bell"
        );
    }

    /// A child killed by a signal is not a child that exited, and the
    /// diagnostic is the only place that distinction survives: `ExitStatus`
    /// gives `None` for the code, and a terminal that printed "status 0" for
    /// a segfaulting shell would be reporting the opposite of what happened.
    #[test]
    fn a_child_killed_by_a_signal_is_not_reported_as_an_exit() {
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let (sender, events) = sync_channel(MAX_PENDING_EVENTS);
        let account = pty::Account {
            uid: 0,
            name: "td-term-test".into(),
            home: directory.display().to_string(),
        };
        let (_threads, _child) = start_child(
            &pty,
            &sender,
            &fixture_command("term_client_abort_fixture"),
            &account,
            pty::Input::new(),
        )
        .unwrap();

        let status = loop {
            match events.recv_timeout(Duration::from_secs(30)).unwrap() {
                Event::Exit(status) => break status,
                Event::Output(_) | Event::Drained => {}
                other => panic!("the child produced {}", named(&other)),
            }
        };
        assert_eq!(status.code(), None, "the fixture exited rather than dying");
        let ended = ended(status);
        assert!(ended.contains("killed by a signal"), "{ended}");
    }

    /// The child half of the test above: the smallest death that is not an
    /// exit. `abort` rather than a signal sent by hand, since sending one
    /// needs `kill(2)` and this crate has no such surface.
    #[test]
    #[ignore = "spawned as the child of a_child_killed_by_a_signal_is_not_reported_as_an_exit"]
    fn term_client_abort_fixture() {
        std::process::abort();
    }

    /// Both translations, including the two arms a real failure would take.
    /// The failing arms are otherwise unreachable from a test — inducing a
    /// read errno other than EIO, or a `wait` that fails, needs a kernel
    /// state this suite cannot arrange — so the mapping is what is pinned:
    /// a fault must become a `Closed`, which ends the terminal with a
    /// diagnostic, and not an ending that looks ordinary.
    #[test]
    fn a_producers_fault_becomes_a_closed_session_rather_than_a_quiet_end() {
        assert!(matches!(
            from_output(pty::Output::Bytes(b"hi".to_vec())),
            Event::Output(bytes) if bytes == b"hi"
        ));
        assert!(matches!(
            from_output(pty::Output::Ended(Ok(()))),
            Event::Drained
        ));
        assert!(matches!(
            from_output(pty::Output::Ended(Err("read terminal: EBADF".into()))),
            Event::Closed(error) if error == "read terminal: EBADF"
        ));
        assert!(matches!(
            from_waited(pty::Waited::Failed("wait for child: ECHILD".into())),
            Event::Closed(error) if error == "wait for child: ECHILD"
        ));
    }

    /// Every `wl_pointer` arm, driven with the arguments the protocol says it
    /// carries. A miscounted argument is silent here and fatal at runtime:
    /// `finish()` rejects the leftover, `dispatch` returns `Err`, and the
    /// terminal EXITS — on `enter`, the first time a pointer crosses its
    /// window. The demo tests all of its own for the same reason.
    #[test]
    fn every_pointer_event_is_decoded_with_the_arguments_it_carries() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        let mut capabilities = Vec::new();
        capabilities.extend_from_slice(&(SEAT_KEYBOARD | SEAT_POINTER).to_ne_bytes());
        surface
            .dispatch(&mut connection, &message(SEAT, 0, capabilities), fallback)
            .unwrap();
        let pointer = surface.pointer.expect("the seat offered a pointer");

        // (opcode, word count). Every one the seat's version range can send:
        // enter, leave, motion, button, axis, frame, axis_source, axis_stop,
        // axis_discrete. `wl_fixed` and `int` are both one word, so a count
        // is what the decode has to agree about.
        // `surface` is the index of the word carrying a surface id where the
        // event has one, since enter and leave refuse another client's.
        for (opcode, words, surface_at) in [
            (0u16, 4usize, Some(1usize)),
            (1, 2, Some(1)),
            (2, 3, None),
            (3, 4, None),
            (4, 3, None),
            (5, 0, None),
            (6, 1, None),
            (7, 2, None),
            (8, 2, None),
        ] {
            let word_at = |word: usize| match surface_at {
                Some(at) if at == word => SURFACE,
                _ => u32::try_from(word).unwrap(),
            };
            let mut payload = Vec::new();
            for word in 0..words {
                payload.extend_from_slice(&word_at(word).to_ne_bytes());
            }
            surface
                .dispatch(
                    &mut connection,
                    &message(pointer, opcode, payload),
                    fallback,
                )
                .unwrap_or_else(|error| panic!("wl_pointer opcode {opcode}: {error}"));

            // And one word too many is REFUSED rather than ignored, which is
            // what makes the count above an assertion rather than a lower
            // bound: a decode reading fewer arguments than arrived would
            // accept both and drift silently against the compositor.
            let mut long = Vec::new();
            for word in 0..words.saturating_add(1) {
                long.extend_from_slice(&word_at(word).to_ne_bytes());
            }
            assert!(
                surface
                    .dispatch(&mut connection, &message(pointer, opcode, long), fallback)
                    .is_err(),
                "wl_pointer opcode {opcode} accepted an extra argument"
            );
        }

        // Another client's surface is REFUSED on the two events that name
        // one, as the keyboard's are: a compositor confusing this client with
        // another is a fault to report rather than a pointer to act on.
        for opcode in [0u16, 1] {
            let mut payload = Vec::new();
            payload.extend_from_slice(&1u32.to_ne_bytes());
            payload.extend_from_slice(&SURFACE.saturating_add(1).to_ne_bytes());
            for _ in 2..if opcode == 0 { 4 } else { 2 } {
                payload.extend_from_slice(&0u32.to_ne_bytes());
            }
            assert!(
                surface
                    .dispatch(
                        &mut connection,
                        &message(pointer, opcode, payload),
                        fallback
                    )
                    .is_err(),
                "wl_pointer opcode {opcode} accepted another client's surface"
            );
        }

        // An opcode the seat's version range cannot produce is fatal rather
        // than dropped: `axis_value120` is version 8 and the seat is bound at
        // 7 or below, so one arriving means the compositor and this client
        // disagree about the version — which is worse than an event lost.
        assert!(surface
            .dispatch(&mut connection, &message(pointer, 9, Vec::new()), fallback)
            .is_err());
    }

    #[test]
    fn a_wheel_scrolls_the_history_and_a_frame_is_what_applies_it() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        surface.cells = Some((4, 8));
        let mut terminal = Terminal::new(4, 8).unwrap();
        for line in 0..40u32 {
            terminal.feed(format!("L{line}\r\n").as_bytes());
        }
        surface.history = terminal.scrollback();
        assert_eq!(surface.viewport.offset(surface.history), 0);

        // The seat announces a pointer, which is what gives it an id — a
        // DYNAMIC one, so nothing here may assume which.
        let mut capabilities = Vec::new();
        capabilities.extend_from_slice(&(SEAT_KEYBOARD | SEAT_POINTER).to_ne_bytes());
        surface
            .dispatch(&mut connection, &message(SEAT, 0, capabilities), fallback)
            .unwrap();
        let pointer = surface.pointer.expect("the seat offered a pointer");

        // One notch AWAY from the operator. The compositor sends that as a
        // negative discrete count — the protocol's sign, where positive is a
        // downward movement of the surface's content — so it goes BACK into
        // history.
        let discrete = |axis: u32, count: i32| {
            let mut payload = Vec::new();
            payload.extend_from_slice(&axis.to_ne_bytes());
            payload.extend_from_slice(&count.to_ne_bytes());
            message(pointer, 8, payload)
        };
        let frame = move || message(pointer, 5, Vec::new());
        surface
            .dispatch(&mut connection, &discrete(0, -1), fallback)
            .unwrap();
        // Nothing yet: a frame is the transaction, so a tilting wheel does
        // not move the view twice for one flick.
        assert_eq!(
            surface.viewport.offset(surface.history),
            0,
            "the wheel moved the view before its frame closed"
        );
        surface
            .dispatch(&mut connection, &frame(), fallback)
            .unwrap();
        // The literal, not the constant: every other assertion here
        // multiplies by the same one, so they would agree at any value.
        assert_eq!(LINES_PER_NOTCH, 3);
        assert_eq!(
            surface.viewport.offset(surface.history),
            usize::try_from(LINES_PER_NOTCH).unwrap()
        );

        // Notches accumulate WITHIN a frame rather than each applying: a fast
        // flick arrives as several in one report.
        surface
            .dispatch(&mut connection, &discrete(0, -2), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &discrete(0, -1), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &frame(), fallback)
            .unwrap();
        assert_eq!(
            surface.viewport.offset(surface.history),
            usize::try_from(LINES_PER_NOTCH * 4).unwrap()
        );

        // The other direction returns toward the live bottom.
        surface
            .dispatch(&mut connection, &discrete(0, 4), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &frame(), fallback)
            .unwrap();
        assert_eq!(surface.viewport.offset(surface.history), 0);
        assert!(!surface.viewport.viewing(surface.history));

        // HORIZONTAL is read and ignored. A terminal has no sideways
        // scrollback, and a tilting wheel sends both — counted together, a
        // sideways flick would scroll up the history.
        surface
            .dispatch(&mut connection, &discrete(1, -9), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &frame(), fallback)
            .unwrap();
        assert_eq!(
            surface.viewport.offset(surface.history),
            0,
            "a sideways flick scrolled the history"
        );
    }

    /// The seat's capabilities are what make the terminal ask for its
    /// devices, and it asks exactly once for each. A seat may re-announce — a
    /// device arriving makes it — and a second `get_keyboard` into the same
    /// object id is a protocol error the compositor answers by closing the
    /// client; a second `get_pointer` would burn an id besides.
    #[test]
    fn a_seat_is_asked_once_for_each_device_however_often_it_announces_them() {
        let (mut connection, mut peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        for _ in 0..3 {
            let mut payload = Vec::new();
            payload.extend_from_slice(&(SEAT_KEYBOARD | 1).to_ne_bytes());
            surface
                .dispatch(&mut connection, &message(SEAT, 0, payload), fallback)
                .unwrap();
        }
        assert!(surface.keyboard_requested);
        // wl_seat.get_keyboard and get_pointer, each carrying the id its own
        // events will arrive on. Three announcements, two requests.
        let (object, opcode, payload) = said(&mut peer);
        assert_eq!((object, opcode), (SEAT, 1));
        assert_eq!(payload, KEYBOARD.to_ne_bytes());
        let (object, opcode, payload) = said(&mut peer);
        assert_eq!((object, opcode), (SEAT, 0));
        // A DYNAMIC id, and the first one this connection hands out: the
        // pointer is not a fixed object, so it takes the id after the fixed
        // range rather than one reserved inside it.
        assert_eq!(surface.pointer, Some(FIRST_DYNAMIC_ID));
        assert_eq!(payload, FIRST_DYNAMIC_ID.to_ne_bytes());
        // And nothing after them. `pair` sets a read timeout precisely so a
        // repeat request is a failure here rather than a hang.
        peer.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut spare = [0u8; 1];
        assert!(
            std::io::Read::read_exact(&mut peer, &mut spare).is_err(),
            "the terminal asked for a device twice"
        );
    }

    /// Each device is asked for only where the seat OFFERS it, and the two
    /// bits are pinned by value like every other protocol number here: read
    /// as each other, the terminal would ask a seat that has no keyboard for
    /// one and never ask a seat that does. Wayland makes a `get_pointer` on a
    /// seat without the capability a protocol error, so the pointer half is
    /// the difference between a terminal that starts and one that is
    /// disconnected on a keyboard-only seat.
    #[test]
    fn a_seat_without_a_keyboard_is_not_asked_for_one() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        // Pointer only, which is capability 1.
        let mut payload = Vec::new();
        payload.extend_from_slice(&SEAT_POINTER.to_ne_bytes());
        surface
            .dispatch(&mut connection, &message(SEAT, 0, payload), fallback)
            .unwrap();
        assert!(!surface.keyboard_requested);
        assert!(surface.pointer.is_some());

        // And the other way round on a fresh seat: a KEYBOARD-only one is
        // asked for no pointer, which is what keeps the terminal usable on a
        // machine with no mouse rather than disconnected from it.
        let mut keyboard_only = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let mut payload = Vec::new();
        payload.extend_from_slice(&SEAT_KEYBOARD.to_ne_bytes());
        keyboard_only
            .dispatch(&mut connection, &message(SEAT, 0, payload), fallback)
            .unwrap();
        assert!(keyboard_only.keyboard_requested);
        assert!(keyboard_only.pointer.is_none());
    }

    /// A frame is not enough. The terminal is not presented until its keymap
    /// has been matched, because what follows is a child on a terminal.
    #[test]
    fn a_frame_without_a_verified_keymap_is_not_presented() {
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.layout_configured = true;
        surface.current = Some(Size {
            width: 8,
            height: 8,
        });
        surface.drawn = surface.wanted();
        surface.frame = Some(frame(true, true));
        surface.seat_capabilities = Some(SEAT_KEYBOARD);
        assert!(surface.ready(), "the fixture is not frame-ready");
        assert!(
            !surface.presented(),
            "a terminal with no verified keymap presented"
        );
        surface.keymap_verified = true;
        assert!(surface.presented());
    }

    /// The other half of that gate. A keymap verified against a seat that no
    /// longer offers a keyboard is a terminal nobody can type into, and the
    /// seat is entitled to withdraw the capability it announced.
    #[test]
    fn a_seat_that_withdraws_its_keyboard_is_not_presented() {
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.layout_configured = true;
        surface.current = Some(Size {
            width: 8,
            height: 8,
        });
        surface.drawn = surface.wanted();
        surface.frame = Some(frame(true, true));
        surface.keymap_verified = true;
        assert!(
            !surface.presented(),
            "a terminal presented before its seat said anything"
        );
        surface.seat_capabilities = Some(SEAT_KEYBOARD);
        assert!(surface.presented(), "the fixture never became presentable");
        surface.seat_capabilities = Some(0);
        assert!(
            !surface.presented(),
            "a terminal whose seat withdrew its keyboard presented"
        );
    }

    /// The wiring end to end: a key event arriving as a Wayland message
    /// leaves bytes in the queue the WRITER drains. Tested through
    /// `serve_event` rather than `dispatch` because the routing — taking what
    /// dispatch translated and pushing it at the child — is production code
    /// no `dispatch` test reaches.
    #[test]
    fn a_key_event_reaches_the_child_through_serve_event() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        let mut model = None;
        let mut child = Child::default();
        let mut payload = Vec::new();
        for word in [1u32, 0, 30, 1] {
            payload.extend_from_slice(&word.to_ne_bytes());
        }
        serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut child,
            Event::Wayland(message(KEYBOARD, 3, payload)),
        )
        .unwrap();
        assert_eq!(
            child.input.take_for_test(),
            b"a".to_vec(),
            "a pressed key did not reach the writer's queue"
        );
        assert_eq!(surface.pending_input, None, "the key was routed twice");

        // The mode the CHILD set has to reach the next key, and only
        // `serve_event` can carry it there — `dispatch` cannot reach the
        // model. Driven through the model rather than by setting the field,
        // or the refresh itself is what goes untested.
        let mut terminal = Terminal::new(4, 8).unwrap();
        terminal.feed(b"\x1b[?1h");
        assert_eq!(terminal.mode("application-cursor"), Some(true));
        let mut model = Some(terminal);
        let mut payload = Vec::new();
        for word in [1u32, 0, 103, 1] {
            payload.extend_from_slice(&word.to_ne_bytes());
        }
        serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut child,
            Event::Wayland(message(KEYBOARD, 3, payload)),
        )
        .unwrap();
        assert_eq!(
            child.input.take_for_test(),
            b"\x1bOA".to_vec(),
            "the model's mode did not reach the key that followed it"
        );

        // A key the queue will not take rings the bell AND asks for the frame
        // that would show it. §10 answers a full queue with the bell rather
        // than by growing it, and a bell nothing redraws is not an answer.
        // Closing the queue is the deterministic refusal; a full one is the
        // same `Ok(false)` after 64KB of keys.
        child.input.close().unwrap();
        surface.stale = false;
        let mut payload = Vec::new();
        for word in [1u32, 0, 30, 1] {
            payload.extend_from_slice(&word.to_ne_bytes());
        }
        serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut child,
            Event::Wayland(message(KEYBOARD, 3, payload)),
        )
        .unwrap();
        assert!(
            model.as_mut().is_some_and(Terminal::take_bell),
            "a refused key did not ring the bell"
        );
        assert!(surface.stale, "a refused key scheduled no redraw");
    }

    /// §11 has the compositor PUBLISH its repeat timings and td-term read
    /// them, so a client that guessed would drift from the server. Rate is
    /// keys per second, and its two edge values both have protocol meanings
    /// that a plain division would get wrong.
    #[test]
    fn repeat_timings_come_from_the_compositors_own_numbers() {
        // The published pair, spelled as numbers: 25 keys per second is a
        // 40ms interval, and pinning it here is what would catch a rate read
        // as an interval.
        let mut published = repeat_from(25, 600);
        published.press(30, 0, keys::Modes::default(), false, 1_000);
        assert_eq!(published.deadline(), Some(1_600), "the delay is not 600ms");
        assert!(
            published
                .due(1_599, keys::Modes::default(), false)
                .is_none(),
            "a repetition came due before its delay"
        );
        assert!(published
            .due(1_600, keys::Modes::default(), false)
            .is_some());
        assert_eq!(
            published.deadline(),
            Some(1_640),
            "25 keys per second is not a 40ms interval"
        );

        // A published DELAY of zero would arm a repeat already due, and the
        // loop dates arming and serving from one `now` — so a single tap
        // would send its byte twice.
        let mut instant = repeat_from(25, 0);
        instant.press(30, 0, keys::Modes::default(), false, 0);
        assert!(
            instant.due(0, keys::Modes::default(), false).is_none(),
            "a tap repeated in the instant it was pressed"
        );

        // Rate zero is the protocol's "do not repeat", not an infinite one.
        let mut none = repeat_from(0, 600);
        none.press(30, 0, keys::Modes::default(), false, 0);
        assert!(!none.armed(), "a zero rate armed a repeat");

        // Neither field has a meaning below zero, so nonsense falls back to
        // the pinned default rather than to a saturated guess.
        // A delay DIFFERENT from the pinned default, or the assertion cannot
        // tell "fell back" from "kept what was published"; and the interval
        // is checked too, since the rate is the half that decides it.
        let mut negative = repeat_from(-1, 111);
        negative.press(30, 0, keys::Modes::default(), false, 0);
        assert_eq!(negative.deadline(), Some(keys::REPEAT_DELAY_MS));
        assert!(negative
            .due(keys::REPEAT_DELAY_MS, keys::Modes::default(), false)
            .is_some());
        assert_eq!(
            negative.deadline(),
            Some(keys::REPEAT_DELAY_MS + keys::REPEAT_INTERVAL_MS),
            "a negative rate did not fall back to the pinned interval"
        );
    }

    /// The repeat wiring: `dispatch` reads the key, the clocked caller dates
    /// it, and a repetition reaches the child through the same queue a fresh
    /// press does. Driven with an injected clock, so this never sleeps.
    #[test]
    fn a_held_key_repeats_into_the_child() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        let mut child = Child::default();
        let mut model = None;
        let key = |code: u32, state: u32| {
            let mut payload = Vec::new();
            for word in [1, 0, code, state] {
                payload.extend_from_slice(&word.to_ne_bytes());
            }
            message(KEYBOARD, 3, payload)
        };
        surface.repeat = repeat_from(25, 600);

        surface
            .dispatch(&mut connection, &key(30, 1), fallback)
            .unwrap();
        arm_repeat(&mut surface, 1_000);
        assert_eq!(
            surface.repeat.deadline(),
            Some(1_600),
            "a press did not arm the repeat"
        );
        // Nothing before the delay, and the first byte only after it.
        serve_repeat(&mut surface, &mut model, &mut child, 1_599).unwrap();
        assert!(
            child.input.take_for_test().is_empty(),
            "a repetition arrived before its delay"
        );
        serve_repeat(&mut surface, &mut model, &mut child, 1_600).unwrap();
        assert_eq!(child.input.take_for_test(), b"a".to_vec());

        // A release retires it; the loop then has no deadline to wait on.
        surface
            .dispatch(&mut connection, &key(30, 0), fallback)
            .unwrap();
        arm_repeat(&mut surface, 1_700);
        assert_eq!(
            surface.repeat.deadline(),
            None,
            "releasing the held key left it repeating"
        );
        serve_repeat(&mut surface, &mut model, &mut child, 9_999).unwrap();
        assert!(
            child.input.take_for_test().is_empty(),
            "a released key repeated anyway"
        );

        // Two things retire a held key besides releasing it, and both are
        // about the sequence armed no longer being the one the key sends: a
        // MODIFIER change, and losing focus, which obliges a client to treat
        // every key as lifted.
        let modifiers = |mask: u32, group: u32| {
            let mut payload = Vec::new();
            for word in [1, mask, 0, 0, group] {
                payload.extend_from_slice(&word.to_ne_bytes());
            }
            message(KEYBOARD, 4, payload)
        };
        let leave = {
            let mut payload = Vec::new();
            for word in [1u32, SURFACE] {
                payload.extend_from_slice(&word.to_ne_bytes());
            }
            message(KEYBOARD, 2, payload)
        };
        for (retire, what) in [
            (
                modifiers(crate::keyboard::MOD_CONTROL, 0),
                "a modifier change",
            ),
            (leave, "losing focus"),
        ] {
            surface
                .dispatch(&mut connection, &key(30, 1), fallback)
                .unwrap();
            arm_repeat(&mut surface, 2_000);
            assert!(surface.repeat.armed(), "the fixture never armed");
            surface
                .dispatch(&mut connection, &retire, fallback)
                .unwrap();
            assert!(
                !surface.repeat.armed(),
                "{what} left the key repeating under the wrong spelling"
            );
        }

        // A layout change, with the modifier mask held CONSTANT so the
        // modifier cancel above cannot be what fires. Reached separately for
        // that reason: driving it in the loop changed both at once, and the
        // group half passed on the other's work.
        surface
            .dispatch(&mut connection, &modifiers(0, 0), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &key(30, 1), fallback)
            .unwrap();
        arm_repeat(&mut surface, 3_000);
        assert!(surface.repeat.armed(), "the layout fixture never armed");
        surface
            .dispatch(&mut connection, &modifiers(0, 1), fallback)
            .unwrap();
        assert!(
            !surface.repeat.armed(),
            "a layout change left the key repeating under td's own map"
        );
        // And a press made WHILE in that group arms nothing, or the repeat
        // would send bytes the single press was refused for.
        surface
            .dispatch(&mut connection, &key(30, 1), fallback)
            .unwrap();
        arm_repeat(&mut surface, 4_000);
        assert!(
            !surface.repeat.armed(),
            "a key pressed in a foreign group armed a repeat"
        );

        // End with the view CLOSED is the child's key. Read as `viewing`
        // when nothing is being viewed, it would route to the viewport
        // instead and a held End would stop reaching the child.
        surface
            .dispatch(&mut connection, &modifiers(0, 0), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &key(107, 1), fallback)
            .unwrap();
        arm_repeat(&mut surface, 5_000);
        assert!(surface.repeat.armed(), "a held End armed nothing");
        let _ = child.input.take_for_test();
        serve_repeat(&mut surface, &mut model, &mut child, 5_600).unwrap();
        assert_eq!(
            child.input.take_for_test(),
            b"\x1b[F".to_vec(),
            "a held End did not keep reaching the child"
        );

        // The guard's other side: an IDENTICAL modifiers message changes
        // nothing and must leave the held key alone. td's server dedupes,
        // but one that re-sent modifiers with every key would otherwise kill
        // autorepeat outright.
        surface
            .dispatch(&mut connection, &modifiers(0, 0), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &key(30, 1), fallback)
            .unwrap();
        arm_repeat(&mut surface, 4_500);
        assert!(surface.repeat.armed(), "the repeat fixture never armed");
        surface
            .dispatch(&mut connection, &modifiers(0, 0), fallback)
            .unwrap();
        assert!(
            surface.repeat.armed(),
            "an unchanged modifiers message retired a held key"
        );
    }

    /// A held key outlives the child's mode changes, which is why `Repeat`
    /// stores a code rather than a sequence — and why a repetition reads the
    /// mode in force NOW rather than the one cached when the key went down.
    #[test]
    fn a_held_cursor_key_follows_the_childs_mode_changes() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        let mut child = Child::default();
        let mut model = Some(Terminal::new(4, 8).unwrap());
        surface.repeat = repeat_from(25, 600);
        let mut payload = Vec::new();
        // evdev 103 is Up, which spells differently under DECCKM.
        for word in [1u32, 0, 103, 1] {
            payload.extend_from_slice(&word.to_ne_bytes());
        }
        surface
            .dispatch(&mut connection, &message(KEYBOARD, 3, payload), fallback)
            .unwrap();
        arm_repeat(&mut surface, 0);
        serve_repeat(&mut surface, &mut model, &mut child, 600).unwrap();
        assert_eq!(child.input.take_for_test(), b"\x1b[A".to_vec());
        // The child turns DECCKM on with no key touched in between.
        if let Some(terminal) = model.as_mut() {
            terminal.feed(b"\x1b[?1h");
        }
        serve_repeat(&mut surface, &mut model, &mut child, 640).unwrap();
        assert_eq!(
            child.input.take_for_test(),
            b"\x1bOA".to_vec(),
            "a held key kept the spelling it was pressed under"
        );
    }

    /// `repeat_info` may arrive at any time, and a rate change is not a reason
    /// to stop repeating the key someone is holding. A rate of ZERO is.
    #[test]
    fn retiming_keeps_the_held_key_but_a_zero_rate_retires_it() {
        let mut repeat = repeat_from(25, 600);
        repeat.press(30, 0, keys::Modes::default(), false, 0);
        assert_eq!(repeat.deadline(), Some(600));
        repeat.retime(&repeat_from(50, 200));
        assert!(
            repeat.armed(),
            "a new rate dropped the key that was being held"
        );
        assert!(repeat.due(600, keys::Modes::default(), false).is_some());
        assert_eq!(
            repeat.deadline(),
            Some(620),
            "the new interval did not take"
        );
        repeat.retime(&repeat_from(0, 600));
        assert!(!repeat.armed(), "a zero rate left a key repeating");
        // The DELAY half of a retime, which only a press made after it can
        // see: the case above presses first, so it pins the interval alone.
        let mut delayed = repeat_from(25, 600);
        delayed.retime(&repeat_from(25, 50));
        delayed.press(30, 0, keys::Modes::default(), false, 0);
        assert_eq!(
            delayed.deadline(),
            Some(50),
            "a retimed delay did not reach the next press"
        );
    }

    /// The offset has to reach the RENDERER, not just the viewport: every
    /// assertion about where the view sits is about a number, and a picture
    /// drawn from the live bottom regardless would satisfy all of them.
    #[test]
    fn a_scrolled_view_draws_a_different_picture() {
        let font = font();
        let palette = render::Palette::pinned();
        let size = Size {
            width: 8 * 8,
            height: 16 * 4,
        };
        let mut terminal = Terminal::new(4, 8).unwrap();
        // Cursor hidden in the MODEL, so the override a scrolled view applies
        // changes nothing and the offset is the only difference between the
        // two frames. Without this the pictures differ either way and the
        // assertion passes with the renderer ignoring the offset entirely.
        terminal.feed(b"\x1b[?25l");
        for line in 0..20u32 {
            terminal.feed(format!("L{line}\r\n").as_bytes());
        }
        let live = build_pixels(size, &terminal, &font, &palette, true, 0).unwrap();
        let back = build_pixels(size, &terminal, &font, &palette, true, 4).unwrap();
        assert_ne!(
            live, back,
            "a scrolled viewport drew the live screen anyway"
        );
    }

    /// The frame's offset comes from the viewport AND the terminal being
    /// drawn. Inline in `commit_frame` this composition could be replaced by
    /// zero with the whole suite green, which is the weaker version of the
    /// defect the renderer test was written for.
    #[test]
    fn the_drawn_offset_is_the_viewport_read_against_the_model() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        surface.cells = Some((4, 8));
        let mut terminal = Terminal::new(4, 8).unwrap();
        for line in 0..20u32 {
            terminal.feed(format!("L{line}\r\n").as_bytes());
        }
        surface.history = terminal.scrollback();
        assert_eq!(
            draw_offset(&surface, &terminal),
            0,
            "a closed view drew somewhere other than the live bottom"
        );
        let mut payload = Vec::new();
        for word in [1u32, crate::keyboard::MOD_SHIFT, 0, 0, 0] {
            payload.extend_from_slice(&word.to_ne_bytes());
        }
        surface
            .dispatch(&mut connection, &message(KEYBOARD, 4, payload), fallback)
            .unwrap();
        let mut payload = Vec::new();
        for word in [1u32, 0, 104, 1] {
            payload.extend_from_slice(&word.to_ne_bytes());
        }
        surface
            .dispatch(&mut connection, &message(KEYBOARD, 3, payload), fallback)
            .unwrap();
        assert_eq!(
            draw_offset(&surface, &terminal),
            surface.viewport.offset(terminal.scrollback()),
            "the drawn offset stopped agreeing with the viewport"
        );
        assert!(
            draw_offset(&surface, &terminal) > 0,
            "an open view still drew the live bottom"
        );
    }

    /// A chord at the oldest end is INERT, and on an empty history that is
    /// not the same as producing no offset today: an anchor written past what
    /// history holds looks inert now and becomes a jump to the oldest line as
    /// soon as the child writes enough to fill it.
    #[test]
    fn a_scroll_against_empty_history_does_not_arm_a_later_jump() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        surface.cells = Some((4, 8));
        let mut terminal = Terminal::new(4, 8).unwrap();
        surface.history = terminal.scrollback();
        assert_eq!(surface.history.lines, 0, "the fixture began with history");

        let mut payload = Vec::new();
        for word in [1u32, crate::keyboard::MOD_SHIFT, 0, 0, 0] {
            payload.extend_from_slice(&word.to_ne_bytes());
        }
        surface
            .dispatch(&mut connection, &message(KEYBOARD, 4, payload), fallback)
            .unwrap();
        let mut payload = Vec::new();
        for word in [1u32, 0, 104, 1] {
            payload.extend_from_slice(&word.to_ne_bytes());
        }
        surface
            .dispatch(&mut connection, &message(KEYBOARD, 3, payload), fallback)
            .unwrap();
        assert!(
            !surface.viewport.viewing(surface.history),
            "a scroll with no history opened a view"
        );

        // The child now writes enough to fill the history that chord was
        // aimed past. A dormant anchor comes into range HERE.
        for line in 0..20u32 {
            terminal.feed(format!("L{line}\r\n").as_bytes());
        }
        surface.history = terminal.scrollback();
        assert!(
            !surface.viewport.viewing(surface.history),
            "output brought a dormant anchor into range and opened the view"
        );
    }

    /// §10: ordinary text input returns to the live bottom. Typing at a
    /// scrolled-back screen otherwise echoes somewhere nobody can see, and
    /// the child's reply lands there too.
    #[test]
    fn ordinary_input_returns_the_view_to_the_live_bottom() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        surface.cells = Some((4, 8));
        let mut terminal = Terminal::new(4, 8).unwrap();
        for line in 0..20u32 {
            terminal.feed(format!("L{line}\r\n").as_bytes());
        }
        surface.history = terminal.scrollback();
        let modifiers = |mask: u32| {
            let mut payload = Vec::new();
            for word in [1, mask, 0, 0, 0] {
                payload.extend_from_slice(&word.to_ne_bytes());
            }
            message(KEYBOARD, 4, payload)
        };
        let key = |code: u32| {
            let mut payload = Vec::new();
            for word in [1u32, 0, code, 1] {
                payload.extend_from_slice(&word.to_ne_bytes());
            }
            message(KEYBOARD, 3, payload)
        };
        surface
            .dispatch(
                &mut connection,
                &modifiers(crate::keyboard::MOD_SHIFT),
                fallback,
            )
            .unwrap();
        surface
            .dispatch(&mut connection, &key(104), fallback)
            .unwrap();
        assert!(
            surface.viewport.viewing(surface.history),
            "the fixture never opened a view"
        );
        surface
            .dispatch(&mut connection, &modifiers(0), fallback)
            .unwrap();
        surface
            .dispatch(&mut connection, &key(30), fallback)
            .unwrap();
        assert_eq!(
            surface.pending_input.take().map(|s| s.as_slice().to_vec()),
            Some(b"a".to_vec()),
            "the letter did not reach the child"
        );
        assert!(
            !surface.viewport.viewing(surface.history),
            "ordinary text input did not return the view to the live bottom"
        );
    }

    /// Shift+PageUp moves the VIEW rather than reaching the child, and a key
    /// that scrolls must not also send bytes — which is why `keys` answers an
    /// enum rather than an optional sequence. The wiring under test is that
    /// the action reaches the viewport at all, and that the frame showing it
    /// gets asked for.
    #[test]
    fn shift_pageup_moves_the_view_instead_of_reaching_the_child() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        surface.cells = Some((4, 8));
        // Twenty lines of history to walk back through, and the numbers the
        // viewport clamps against read from the same model.
        let mut terminal = Terminal::new(4, 8).unwrap();
        for line in 0..20u32 {
            terminal.feed(format!("L{line}\r\n").as_bytes());
        }
        surface.history = terminal.scrollback();
        assert!(
            surface.history.lines > 0,
            "the fixture produced no scrollback"
        );

        let mut payload = Vec::new();
        for word in [1u32, crate::keyboard::MOD_SHIFT, 0, 0, 0] {
            payload.extend_from_slice(&word.to_ne_bytes());
        }
        surface
            .dispatch(&mut connection, &message(KEYBOARD, 4, payload), fallback)
            .unwrap();
        let mut payload = Vec::new();
        // evdev 104 is PageUp.
        for word in [1u32, 0, 104, 1] {
            payload.extend_from_slice(&word.to_ne_bytes());
        }
        surface.stale = false;
        surface
            .dispatch(&mut connection, &message(KEYBOARD, 3, payload), fallback)
            .unwrap();
        assert_eq!(
            surface.pending_input, None,
            "a key that scrolls also sent bytes to the child"
        );
        assert!(
            surface.viewport.offset(surface.history) > 0,
            "Shift+PageUp did not move the view"
        );
        assert!(surface.stale, "a moved view asked for no frame");
        assert!(
            surface.viewport.viewing(surface.history),
            "the view moved but does not report itself as open"
        );

        // With the view open, End means "back to the bottom" rather than the
        // sequence it sends at the live screen — which is the `viewing`
        // argument doing its job, and the reason it is read rather than
        // assumed.
        let end = {
            let mut payload = Vec::new();
            for word in [1u32, 0, 107, 1] {
                payload.extend_from_slice(&word.to_ne_bytes());
            }
            message(KEYBOARD, 3, payload)
        };
        // Shift is still held from the PageUp above, and Shift on a
        // navigation key is silent — so it is released first, or this would
        // pin nothing.
        let mut released = Vec::new();
        for word in [1u32, 0, 0, 0, 0] {
            released.extend_from_slice(&word.to_ne_bytes());
        }
        surface
            .dispatch(&mut connection, &message(KEYBOARD, 4, released), fallback)
            .unwrap();
        surface.dispatch(&mut connection, &end, fallback).unwrap();
        assert_eq!(
            surface.pending_input, None,
            "End reached the child while the view was scrolled back"
        );
        assert!(
            !surface.viewport.viewing(surface.history),
            "End did not return the view to the live bottom"
        );
        // And at the bottom it is the child's again.
        surface.dispatch(&mut connection, &end, fallback).unwrap();
        assert_eq!(
            surface.pending_input.take().map(|s| s.as_slice().to_vec()),
            Some(b"\x1b[F".to_vec()),
            "End stopped reaching the child once the view was closed"
        );
    }

    /// The whole point of the seat: a pressed key becomes the bytes a child
    /// reads. `keys` owns the table, so this holds the WIRING to it — that
    /// the code, the folded modifier mask and the terminal's mode all reach
    /// `keys::action`, and that what comes back is what the child is offered.
    #[test]
    fn a_pressed_key_becomes_the_bytes_the_child_reads() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        let modifiers = |depressed: u32, latched: u32, locked: u32, group: u32| {
            let mut payload = Vec::new();
            for word in [1, depressed, latched, locked, group] {
                payload.extend_from_slice(&word.to_ne_bytes());
            }
            message(KEYBOARD, 4, payload)
        };
        let key = |code: u32, state: u32| {
            let mut payload = Vec::new();
            for word in [1, 0, code, state] {
                payload.extend_from_slice(&word.to_ne_bytes());
            }
            message(KEYBOARD, 3, payload)
        };
        // evdev 30 is `a`; MOD_CONTROL is what makes it Ctrl-A. Spelled as
        // numbers so the test pins the wiring rather than restating it.
        let press = |surface: &mut Surface, connection: &mut Connection, code: u32| {
            surface
                .dispatch(connection, &key(code, 1), fallback)
                .unwrap();
            surface.pending_input.take().map(|s| s.as_slice().to_vec())
        };
        assert_eq!(
            press(&mut surface, &mut connection, 30),
            Some(b"a".to_vec()),
            "an unmodified letter did not reach the child"
        );
        // A release sends nothing, or every key would arrive twice.
        surface
            .dispatch(&mut connection, &key(30, 0), fallback)
            .unwrap();
        assert_eq!(surface.pending_input, None, "a key release sent bytes");
        // Each of the three masks alone must reach the translation: Wayland
        // reports a held, a latched and a locked modifier in separate words,
        // and folding them is this client's job, so a fold that dropped one
        // would leave Ctrl working only while it was physically held down.
        for (depressed, latched, locked, which) in [
            (crate::keyboard::MOD_CONTROL, 0, 0, "depressed"),
            (0, crate::keyboard::MOD_CONTROL, 0, "latched"),
            (0, 0, crate::keyboard::MOD_CONTROL, "locked"),
        ] {
            surface
                .dispatch(
                    &mut connection,
                    &modifiers(depressed, latched, locked, 0),
                    fallback,
                )
                .unwrap();
            assert_eq!(
                press(&mut surface, &mut connection, 30),
                Some(vec![0x01]),
                "a {which} modifier did not reach the translation"
            );
        }
        // A group this keymap does not have is refused rather than read
        // against group 0's table, which would send a different key's bytes.
        surface
            .dispatch(
                &mut connection,
                &modifiers(crate::keyboard::MOD_CONTROL, 0, 0, 1),
                fallback,
            )
            .unwrap();
        assert_eq!(
            press(&mut surface, &mut connection, 30),
            None,
            "a key in an unknown group was translated anyway"
        );
        // Reported once for that group, and re-armed when the group CHANGES,
        // so leaving td's map a second time is reported a second time rather
        // than silently. Asserted on the flag because the report itself goes
        // to stderr, which a unit test has no handle on.
        assert!(surface.group_reported, "a foreign group went unreported");
        surface
            .dispatch(&mut connection, &modifiers(0, 0, 0, 1), fallback)
            .unwrap();
        assert!(surface.group_reported, "the same group re-armed its report");
        surface
            .dispatch(&mut connection, &modifiers(0, 0, 0, 0), fallback)
            .unwrap();
        assert!(
            !surface.group_reported,
            "returning to td's map kept the report armed"
        );
        assert_eq!(
            press(&mut surface, &mut connection, 30),
            Some(b"a".to_vec()),
            "returning to group 0 did not resume translation"
        );
        // End is the one key whose meaning depends on where the viewport is,
        // and this client passes `viewing: false` because it has no viewport.
        // Asserted because that literal is otherwise pinned by nothing: read
        // as true, End becomes a scroll and stops reaching the shell.
        assert_eq!(
            press(&mut surface, &mut connection, 107),
            Some(b"\x1b[F".to_vec()),
            "End did not reach the child with no viewport to move"
        );
    }

    /// The terminal's mode picks between two spellings of the same key, and
    /// the model that holds it is not `dispatch`'s to reach — so the wiring
    /// that carries it across is what this pins.
    #[test]
    fn application_cursor_mode_reaches_the_translation() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        let mut payload = Vec::new();
        // evdev 103 is Up.
        for word in [1u32, 0, 103, 1] {
            payload.extend_from_slice(&word.to_ne_bytes());
        }
        let up = message(KEYBOARD, 3, payload);
        surface.dispatch(&mut connection, &up, fallback).unwrap();
        assert_eq!(
            surface.pending_input.take().map(|s| s.as_slice().to_vec()),
            Some(b"\x1b[A".to_vec()),
            "normal cursor mode did not spell Up"
        );
        surface.modes = keys::Modes {
            application_cursor: true,
        };
        surface.dispatch(&mut connection, &up, fallback).unwrap();
        assert_eq!(
            surface.pending_input.take().map(|s| s.as_slice().to_vec()),
            Some(b"\x1bOA".to_vec()),
            "application cursor mode did not reach the translation"
        );
    }

    /// Focus in and out. The arms drop what they read, so nothing downstream
    /// would notice their checks going missing — which is the whole reason
    /// they are checked here. A foreign surface id is a compositor confusing
    /// this client with another, and the held-key array is the one field a
    /// compositor states the length of, so both bounds are pinned from either
    /// side rather than by the constant they are read with.
    #[test]
    fn keyboard_focus_events_validate_their_surface_and_key_array() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        let enter = |target: u32, keys: usize, misaligned: bool| {
            let mut payload = Vec::new();
            payload.extend_from_slice(&1u32.to_ne_bytes());
            payload.extend_from_slice(&target.to_ne_bytes());
            let bytes = keys * 4 + usize::from(misaligned);
            payload.extend_from_slice(&(bytes as u32).to_ne_bytes());
            payload.resize(payload.len() + bytes.next_multiple_of(4), 0);
            message(KEYBOARD, 1, payload)
        };
        let leave = |target: u32| {
            let mut payload = Vec::new();
            payload.extend_from_slice(&1u32.to_ne_bytes());
            payload.extend_from_slice(&target.to_ne_bytes());
            message(KEYBOARD, 2, payload)
        };
        // 256 keys is the bound, so it is accepted and 257 is not: spelled in
        // numbers, or the assertion moves with the constant it is checking.
        for (event, want, what) in [
            (enter(SURFACE, 0, false), true, "an empty enter"),
            (enter(SURFACE, 256, false), true, "an enter at the bound"),
            (enter(SURFACE, 257, false), false, "an enter over the bound"),
            (enter(SURFACE, 1, true), false, "a misaligned key array"),
            (enter(SURFACE + 1, 0, false), false, "an enter elsewhere"),
            (leave(SURFACE), true, "a leave"),
            (leave(SURFACE + 1), false, "a leave elsewhere"),
        ] {
            let result = surface.dispatch(&mut connection, &event, fallback);
            assert_eq!(result.is_ok(), want, "{what} was misjudged: {result:?}");
        }
    }

    /// A version-1 wl_keyboard has exactly two key states. A third is a
    /// compositor this client cannot read, and the next landing would have
    /// to guess whether it meant a keystroke.
    #[test]
    fn a_key_event_with_an_impossible_state_is_refused() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        // Spelled in NUMBERS rather than through the constants they are read
        // with, or the test compares each constant with itself.
        for (state, want) in [(0u32, true), (1, true), (2, false)] {
            let mut payload = Vec::new();
            for word in [1, 2, 30, state] {
                payload.extend_from_slice(&word.to_ne_bytes());
            }
            let result =
                surface.dispatch(&mut connection, &message(KEYBOARD, 3, payload), fallback);
            assert_eq!(result.is_ok(), want, "key state {state} was misjudged");
            if !want {
                let error = result.err().unwrap_or_default();
                assert!(error.contains("invalid state 2"), "{error}");
            }
        }
    }

    /// A keymap that is not td's is refused, and refused where §11 wants it
    /// refused: before anything downstream of presentation happens. The
    /// compositor and the terminal are built from one pinned keymap, so a
    /// different one is a compositor this terminal cannot read keys from.
    #[test]
    fn a_keymap_that_is_not_tds_is_refused() {
        let mut wrong = crate::keyboard::XKB_KEYMAP.as_bytes().to_vec();
        wrong.push(0);
        let first = wrong.first().copied().unwrap_or(0);
        if let Some(byte) = wrong.first_mut() {
            *byte = first ^ 1;
        }
        let (mut connection, peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let fallback = Size {
            width: 8,
            height: 8,
        };
        let file = conn::backing_file(&std::env::temp_dir(), "td-term-test", &wrong).unwrap();
        let mut keymap = wire::Builder::new();
        keymap.u32(1);
        keymap.u32(u32::try_from(wrong.len()).unwrap());
        let bytes = keymap.message(KEYBOARD, 0).unwrap();
        conn::send_event_with_fd(&peer, &bytes, &file).unwrap();
        let received = connection.next().unwrap();
        let error = surface
            .dispatch(&mut connection, &received, fallback)
            .err()
            .unwrap();
        assert!(error.contains("differs from td's pinned keymap"), "{error}");
        assert!(!surface.keymap_verified);
        assert_eq!(
            connection.pending_fd_count(),
            0,
            "a refused keymap left its descriptor unclaimed"
        );
    }

    /// A child that is this test binary, running the ignored fixture below.
    /// `current_exe` rather than `/bin/sh` for `pty.rs`'s reason: the gate's
    /// shell is not td's, and a test that depended on one would be testing the
    /// host.
    fn fixture_command(name: &str) -> pty::ChildCommand {
        pty::ChildCommand {
            program: std::env::current_exe().unwrap_or_default(),
            arguments: vec![
                "--exact".into(),
                format!("term_client::tests::{name}").into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
        }
    }

    const CHILD_MARKER: &str = "TD-TERM-CHILD-SPOKE";
    const CHILD_ECHO: &str = "TD-TERM-CHILD-HEARD:";
    const CHILD_TYPED: &str = "ping";

    /// The child half of the tests below. `#[ignore]` is what keeps it out of
    /// an ordinary run: only a parent naming it exactly, with `--ignored`,
    /// ever reaches it. It cannot be gated on the environment as `pty.rs`'s
    /// fixture is, because the environment its parent gives it is the one
    /// `start_child` constructs — which is the production one, and the point.
    #[test]
    #[ignore = "spawned as the child of a_child_exiting_ends_the_loop_and_names_the_status"]
    fn term_client_child_fixture() {
        print!("{CHILD_MARKER}");
        std::io::stdout().flush().unwrap();
    }

    /// The same, and then a LINE READ: it says what it was told, which is what
    /// makes the writer's absence visible from the parent. A whole line,
    /// because the slave is in the kernel's canonical mode and a read there
    /// returns nothing until one is complete.
    #[test]
    #[ignore = "spawned as the child of a_child_puts_its_own_output_on_the_screen"]
    fn term_client_echo_fixture() {
        print!("{CHILD_MARKER}");
        std::io::stdout().flush().unwrap();
        let mut line = String::new();
        let read = std::io::stdin().read_line(&mut line).unwrap();
        assert_ne!(read, 0, "the fixture read end-of-file rather than a line");
        print!("{CHILD_ECHO}{}", line.trim_end());
        std::io::stdout().flush().unwrap();
    }

    /// The whole pipeline, with a real child on a real PTY: the child writes,
    /// the kernel carries it, the reader thread puts it on the loop's
    /// channel, the loop feeds the model, and the model holds the text.
    ///
    /// Every earlier test of this client drew an EMPTY terminal, so a frame
    /// proved the plumbing and nothing about the contents. This is the one
    /// that would notice a loop which served output by throwing it away.
    #[test]
    fn a_child_puts_its_own_output_on_the_screen() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        surface.xrgb = true;
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        surface.current = Some(fallback);
        let mut model = None;
        adopt_size(&mut connection, &mut surface, &mut model, &session).unwrap();
        // The frame the first draw put out comes back, or the redraw output
        // asks for would be throttled behind it forever — there is no
        // compositor here to release anything.
        surface.frame = Some(frame(true, true));

        // Through `start_child` itself, not through its parts: the wiring is
        // what this proves, and an open-coded spawn here would leave a
        // production function nothing in the suite ever calls.
        let (sender, events) = sync_channel(MAX_PENDING_EVENTS);
        let account = pty::Account {
            uid: 0,
            name: "td-term-test".into(),
            home: directory.display().to_string(),
        };
        // The queue is passed IN, not minted here: startup may already have
        // put type-ahead in it, and a writer given a fresh one would drop
        // exactly the keys a person struck while the window was appearing.
        let queued = pty::Input::new();
        let (_threads, mut child) = start_child(
            &pty,
            &sender,
            &fixture_command("term_client_echo_fixture"),
            &account,
            Arc::clone(&queued),
        )
        .unwrap();
        assert!(
            Arc::ptr_eq(&child.input, &queued),
            "the child minted its own queue and abandoned startup's"
        );
        drop(sender);

        let mut typed = false;
        let mut ended = None;
        while ended.is_none() {
            let event = events
                .recv_timeout(Duration::from_secs(30))
                .expect("the terminal never ended with its child");
            // A compositor keeping up, so the redraw each turn asks for is not
            // throttled behind a frame that never comes back. (Serving
            // continues after the marker is on screen, because the ending is
            // what this waits for.)
            surface.frame = Some(frame(true, true));
            let served = serve_event(
                &mut connection,
                &mut surface,
                &mut model,
                &session,
                fallback,
                &mut child,
                event,
            );
            if let Err(error) = served {
                ended = Some(error);
            }
            // Answer the child once it has spoken, which is the return path:
            // this goes into the queue the writer thread drains, and the child
            // reads it back off its own terminal.
            let spoke = model
                .as_ref()
                .is_some_and(|terminal| screen(terminal).contains(CHILD_MARKER));
            if spoke && !typed {
                typed = true;
                assert!(
                    child
                        .input
                        .push(format!("{CHILD_TYPED}\n").as_bytes())
                        .unwrap(),
                    "the queue refused the line to type"
                );
            }
        }
        let text = model.as_ref().map(screen).unwrap_or_default();
        assert!(
            text.contains(CHILD_MARKER),
            "the child's output never reached the model: {text:?} (ended: {ended:?})"
        );
        // And the other direction, which takes the writer thread: what the
        // parent put on the queue reached the child, and the child said so.
        assert!(
            text.contains(&format!("{CHILD_ECHO}{CHILD_TYPED}")),
            "the child never heard what was typed at it: {text:?}"
        );
        // The output made the picture stale and the same turn replaced it.
        assert!(!surface.stale, "the screen was left stale after output");
        assert_eq!(surface.drawn, surface.wanted());
        // And the session ends of its own accord, which takes BOTH threads:
        // the waiter for the status and the reader for the drain. Without one
        // of them this loop runs until the channel disconnects and `ended`
        // stays `None`.
        let ended = ended.expect("the terminal never ended with its child");
        assert!(ended.contains("child exited with status 0"), "{ended}");
    }

    /// A child ending ends the session, and says which way it ended. A
    /// terminal that outlived its shell would be a window nobody can type
    /// into, and one that reported nothing would look like a compositor
    /// failure in the log.
    #[test]
    fn a_child_exiting_ends_the_loop_and_names_the_status() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
            seat: 5,
        });
        let font = font();
        let palette = render::Palette::pinned();
        let pty = Pty::open(Path::new(pty::DEV_PTMX)).unwrap();
        let directory = std::env::temp_dir();
        let session = Session {
            directory: &directory,
            pty: &pty,
            font: &font,
            palette: &palette,
        };
        let fallback = default_size(&font).unwrap();
        let mut model = None;
        let (sender, events) = sync_channel(MAX_PENDING_EVENTS);
        let account = pty::Account {
            uid: 0,
            name: "td-term-test".into(),
            home: directory.display().to_string(),
        };
        let (_threads, mut child) = start_child(
            &pty,
            &sender,
            &fixture_command("term_client_child_fixture"),
            &account,
            pty::Input::new(),
        )
        .unwrap();
        let status = loop {
            match events.recv_timeout(Duration::from_secs(30)).unwrap() {
                Event::Exit(status) => break status,
                Event::Output(_) | Event::Drained => {}
                other => panic!("the child produced {}", named(&other)),
            }
        };
        // The exit ALONE does not end the terminal: the child's last bytes may
        // still be in the kernel, and `Event::Exit` can overtake them.
        serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut child,
            Event::Exit(status),
        )
        .unwrap();
        let ended = serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut child,
            Event::Drained,
        )
        .err()
        .unwrap();
        assert!(ended.contains("child exited with status 0"), "{ended}");
    }

    /// The PTY is opened BEFORE the first frame, so a machine that cannot
    /// provide one fails without having drawn a window. The peer is dropped
    /// here, so the Wayland side fails immediately too — which is what makes
    /// the assertion about WHICH failure is reported a test of the order
    /// rather than of a timeout.
    #[test]
    fn a_pty_that_cannot_be_opened_is_refused_before_the_first_frame() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        let font = font();
        let palette = render::Palette::pinned();
        let mut connection = Connection::over(ours, None, FIRST_DYNAMIC_ID);
        let refused = prepare(
            &mut connection,
            &std::env::temp_dir(),
            &font,
            &palette,
            Path::new("/nonexistent/td-term-ptmx"),
        )
        .err()
        .unwrap();
        assert!(
            refused.contains("/nonexistent/td-term-ptmx"),
            "the terminal reached Wayland before it had a terminal: {refused}"
        );
    }

    #[test]
    fn the_selftest_covers_the_default_grid() {
        selftest().unwrap();
    }
}
