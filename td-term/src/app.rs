//! td-term's window: td-ui's client and turn loop, driving td-ui's terminal
//! model, renderer and keyboard encoder over a PTY.
//!
//! Readiness follows a frame drawn at a size the compositor CHOSE, which is
//! a second frame on td's compositor: it cannot tile a surface it has not
//! mapped, so its first configure is zero in both axes, presenting at the
//! fallback maps the surface, and the tile arrives in the configure after
//! that. That frame must have come back with both its buffer release and
//! its frame callback, and the seat must hold a keyboard whose keymap the
//! toolkit compiled. Only then is the child started, the readiness socket
//! published and the readiness line printed, in that order. The desktop
//! profile (td-term/DESIGN.md §7) starts its child at its first committed
//! frame, since a compositor may keep that buffer or never call back for a
//! window it is not showing, and publishes nothing.
//!
//! The child's output, its exit and the reader's ending arrive from threads
//! on one bounded channel; each producer wakes the turn loop after it sends,
//! so output reaches the screen without the loop polling for it.

use crate::ready;
use crate::session;
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use td_ui::client::{App, Client, ClipboardEvent, Handled, KeyboardEvent, Tag, DISPLAY};
use td_ui::clipboard::{Incoming, Outgoing};
use td_ui::data;
use td_ui::face::{Face, Sizing, MAX_PIXELS_PER_EM, MIN_PIXELS_PER_EM};
use td_ui::keyboard::Stroke;
use td_ui::pointer::{self, Wheel};
use td_ui::pty::{self, Pty};
use td_ui::raster::{MAX_AXIS, MAX_FRAME_BYTES};
use td_ui::vt::Terminal;
use td_ui::vt_keys as keys;
use td_ui::vt_render as render;
use td_ui::wayland::{Endpoint, Waker};
use td_ui::wire::Message;

type Result<T> = std::result::Result<T, String>;

/// What a profile starts: the command, its directory and its whole
/// environment.
type Launch = (pty::ChildCommand, PathBuf, Vec<(OsString, OsString)>);

/// Whose terminal this is (td-term/DESIGN.md §7).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Profile {
    /// td's session program: supervised through its readiness socket, the
    /// child's environment constructed for the verified account.
    Td { ready_socket: PathBuf },
    /// A terminal on a desktop: the inherited environment and the person's
    /// shell, ending with the child's status.
    Desktop,
}

pub struct Options {
    /// The compositor's socket, or `None` for the environment's endpoint.
    pub socket: Option<PathBuf>,
    pub profile: Profile,
    pub working_directory: Option<PathBuf>,
    /// The child's literal argv, or empty for the default shell. See
    /// `session::child_command` for what each means. Bytes rather than text:
    /// a filename argument is whatever the filesystem holds.
    pub command: Vec<OsString>,
    /// The outline face's size in points (`--font-size`), or `None` for
    /// the face fitted to the bitmap face's cell.
    pub font_size: Option<f32>,
}

/// What an operator sees in a title bar; td's compositor keeps it.
pub const TITLE: &str = "td terminal";
const APP_ID: &str = "td-term";

/// Bound on reaching readiness. Past this the compositor is not coming, and
/// a terminal that waits forever is one td-svc reports as down without ever
/// saying why; it is set below the supervisor's 30.
const HANDSHAKE_MS: u64 = 20_000;

/// How long a bell's ring stays on screen after the frame that took it.
const BELL_FLASH_MS: u64 = 100;

/// Where the session's own identity is read from: the uid from the first,
/// everything else from the second.
const PROC_STATUS: &str = "/proc/self/status";
const ETC_PASSWD: &str = "/etc/passwd";
const PROC_CMDLINE: &str = "/proc/cmdline";
const MAX_CMDLINE_BYTES: usize = 4096;
const CLIPBOARD_PROOF_CMDLINE_TOKEN: &[u8] = b"td.firefox-input=1";

const LEFT_BUTTON: u32 = 0x110;
const RIGHT_BUTTON: u32 = 0x111;
const MIDDLE_BUTTON: u32 = 0x112;
/// The most wheel reports one pointer frame sends, so a flung trackpad
/// cannot fill the child's input with them.
const MAX_WHEEL_REPORTS: usize = 10;
/// Presses at one cell this close together are one gesture: a second
/// selects the word, a third the row.
const MULTI_CLICK_MS: u64 = 500;
/// The two chords the terminal keeps for itself rather than sending.
const COPY_CHORD: &str = "C-S-c";
const PASTE_CHORD: &str = "C-S-v";
/// foot's scrollback-search chord, which opens the search; inside it,
/// foot's search chords.
const SEARCH_CHORD: &str = "C-S-r";
const SEARCH_OLDER_CHORDS: [&str; 2] = ["C-r", "C-S-r"];
const SEARCH_NEWER_CHORDS: [&str; 2] = ["C-s", "C-S-s"];
const SEARCH_CANCEL_CHORDS: [&str; 3] = ["Escape", "C-g", "C-c"];
/// td-ui's keymap names keypad Enter `Return` too.
const SEARCH_COMMIT_CHORD: &str = "Return";
const SEARCH_ERASE_CHORD: &str = "Backspace";
/// foot's font chords: Control with `+` or `=` grows the outline face a
/// step, with `-` shrinks it, and with `0` restores the size td-term
/// started at. Each is silent to the child (`vt_keys`), so none is taken
/// from it.
const ZOOM_IN_CHORDS: [&str; 2] = ["C-+", "C-="];
const ZOOM_OUT_CHORD: &str = "C--";
const ZOOM_RESET_CHORD: &str = "C-0";
/// Pixels per em in a point, at the 96 dots per inch foot sizes its font
/// by at scale one, so `--font-size 9` is foot's `size=9`.
const PIXELS_PER_POINT: f32 = 4.0 / 3.0;
const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;
const CLIPBOARD_PROOF_BYTES: &[u8; 7] = b"Welcome";
const CLIPBOARD_TARGET_PREFIX: &str = "TD-TERM-CLIPBOARD-TARGET-READY";
const CLIPBOARD_FOCUS_PREFIX: &str = "TD-TERM-CLIPBOARD-FOCUS-READY serial=";
/// A pending clipboard transfer is stepped at least this often.
const TRANSFER_WAIT_MS: u64 = 50;

/// The grid a terminal falls back to when the compositor proposes no size:
/// what a terminfo entry, a shell prompt and anything drawing a box assume
/// when they cannot ask, turned into pixels by the font.
const DEFAULT_COLUMNS: usize = 80;
const DEFAULT_ROWS: usize = 24;

/// How many events may be in flight before a producer blocks. A blocked
/// PTY reader is how the kernel's own buffer backpressures the child, and
/// the queue's whole length in read chunks is the output ceiling.
const MAX_PENDING_EVENTS: usize = pty::MAX_OUTPUT_CHUNKS;
/// What one turn takes from the child's queue. A child that writes as fast
/// as the reader reads would otherwise hold the turn forever, and with it
/// the keyboard that could interrupt it and the frame that shows it.
const MAX_DRAINED_PER_TURN: usize = 16;
const _: () = assert!(MAX_DRAINED_PER_TURN <= MAX_PENDING_EVENTS);
const _: () = assert!(MAX_PENDING_EVENTS * pty::READ_CHUNK <= pty::MAX_OUTPUT_BYTES);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Size {
    pub width: usize,
    pub height: usize,
}

/// The pixel size of the fallback grid in `cell`s: 80x24, or as much of
/// it as a surface within the raster's ceilings holds, so a large font
/// leaves a smaller window rather than one no frame can be drawn for.
pub fn default_size((cell_width, cell_height): (usize, usize)) -> Result<Size> {
    if cell_width == 0 || cell_height == 0 {
        return Err("the cell has no area".into());
    }
    let columns = DEFAULT_COLUMNS.min(MAX_AXIS / cell_width).max(1);
    let width = columns
        .checked_mul(cell_width)
        .ok_or("the default column count overflows a pixel width")?;
    let row_bytes = width
        .checked_mul(cell_height)
        .and_then(|pixels| pixels.checked_mul(render::BYTES_PER_PIXEL))
        .ok_or("a default row overflows a byte count")?;
    let rows = DEFAULT_ROWS
        .min(MAX_AXIS / cell_height)
        .min(MAX_FRAME_BYTES / row_bytes)
        .max(1);
    let height = rows
        .checked_mul(cell_height)
        .ok_or("the default row count overflows a pixel height")?;
    Ok(Size { width, height })
}

/// The grid of `cell`s a surface of this size holds: `grid_for_tile` is
/// the division the renderer clips to, and `grid_size` the validity the
/// winsize ioctl is held to, so a grid this refuses nothing downstream
/// would take.
pub fn grid(size: Size, (cell_width, cell_height): (usize, usize)) -> Result<(u16, u16)> {
    let (rows, columns) = pty::grid_for_tile(size.width, size.height, cell_width, cell_height)?;
    let window = pty::grid_size(rows, columns)?;
    Ok((window.rows, window.columns))
}

/// A `--font-size` in points, held to the sizes the outline face takes.
pub fn font_points(value: &str) -> Result<f32> {
    let refused = || {
        let pixels = |limit: u16| f32::from(limit) / PIXELS_PER_POINT;
        format!(
            "--font-size '{value}' is not a size from {} to {} points",
            pixels(MIN_PIXELS_PER_EM),
            pixels(MAX_PIXELS_PER_EM)
        )
    };
    let points: f32 = value.parse().map_err(|_| refused())?;
    let range = f32::from(MIN_PIXELS_PER_EM)..=f32::from(MAX_PIXELS_PER_EM);
    if !range.contains(&(points * PIXELS_PER_POINT)) {
        return Err(refused());
    }
    Ok(points)
}

/// How the outline face is sized at startup: at `--font-size` points on
/// its own cell, or fitted to the bitmap face's.
fn start_sizing(font_size: Option<f32>, font: &td_ui::font::Font) -> Sizing {
    match font_size {
        Some(points) => Sizing::PixelsPerEm(points * PIXELS_PER_POINT),
        None => Sizing::Cell {
            width: font.width(),
            height: font.height(),
        },
    }
}

/// Where a font chord moves the outline face, if `chord` is one.
fn zoom_to(chord: &str) -> Option<render::ZoomTo> {
    match chord {
        ZOOM_OUT_CHORD => Some(render::ZoomTo::Out),
        ZOOM_RESET_CHORD => Some(render::ZoomTo::Start),
        _ if ZOOM_IN_CHORDS.contains(&chord) => Some(render::ZoomTo::In),
        _ => None,
    }
}

/// A compositor-supplied size, bounded before anything is allocated for it,
/// the model included.
fn frame_bytes(size: Size) -> Result<usize> {
    if size.width == 0 || size.height == 0 {
        return Err(format!(
            "terminal surface {}x{} has no area",
            size.width, size.height
        ));
    }
    if size.width > MAX_AXIS || size.height > MAX_AXIS {
        return Err(format!(
            "terminal surface {}x{} exceeds {MAX_AXIS}",
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
    if bytes > MAX_FRAME_BYTES {
        return Err(format!(
            "terminal surface needs {bytes} bytes, exceeding {MAX_FRAME_BYTES}"
        ));
    }
    Ok(bytes)
}

/// td-term's own object in the client's table: the `wl_display.sync` that
/// tells the clipboard proof the compositor has taken the selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Object {
    Sync,
    RetiredSync,
}

impl Tag for Object {
    fn retired(self) -> bool {
        self == Object::RetiredSync
    }
}

/// What the child's threads send the loop.
enum Event {
    /// Bytes the child wrote. Whole reads, not lines: the parser is a state
    /// machine and an escape sequence split across two reads is ordinary.
    Output(Vec<u8>),
    /// The child's last slave closed: nothing more will arrive.
    Drained,
    /// The child exited.
    Exit(ExitStatus),
    /// A thread stopping, and why. Nothing joins them, so each reports its
    /// fault here and the loop is where it becomes the process's.
    Closed(String),
}

fn from_output(output: pty::Output) -> Event {
    match output {
        pty::Output::Bytes(bytes) => Event::Output(bytes),
        pty::Output::Ended(Ok(())) => Event::Drained,
        pty::Output::Ended(Err(error)) => Event::Closed(error),
    }
}

fn from_waited(waited: pty::Waited) -> Event {
    match waited {
        pty::Waited::Exited(status) => Event::Exit(status),
        pty::Waited::Failed(error) => Event::Closed(error),
    }
}

/// What a frame was drawn FOR: the size, the activation that decides how
/// the cursor is drawn, so a configure that only takes focus away still
/// needs a new picture, and the link Control-hover underlines, so holding
/// or letting go of Control, or moving off the link, does too without the
/// model having changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Drawn {
    size: Size,
    activated: bool,
    link: Option<render::LinkSpan>,
}

/// The frame in flight: complete once the compositor released its buffer
/// AND its frame callback fired, which mean different things.
struct Frame {
    buffer: u32,
    presented: bool,
    /// The screen shows the model as it stands: this frame's callback
    /// said it reached the screen, or it was drawn for the same model as
    /// one that had (only the hovered link or the focus changed), so
    /// the screen showed that model before it and after.
    shows_model: bool,
}

/// The child, from the loop's side. The terminal ends when its output has
/// run out AND its child has been waited for: the two race, and ending on
/// either alone drops the parting output or names no status.
struct Child {
    drained: bool,
    status: Option<ExitStatus>,
    /// The name a `--command` program's last screen is reported under, and
    /// `None` for the default shell.
    program: Option<String>,
    /// Never joined (see td-ui's `pty`); held so the handles are not dropped
    /// by accident into something that would.
    _threads: Vec<JoinHandle<Result<()>>>,
    /// td's readiness socket; the desktop profile publishes none.
    _ready: Option<ready::Published>,
}

/// What td-term offered on one of the seat's selections: the text behind
/// the live source, and the send in flight.
#[derive(Default)]
struct Offered {
    text: Option<Arc<str>>,
    outgoing: Option<(Outgoing, Arc<str>)>,
}

/// The two selections' offers and the one paste in flight, with the
/// selection it reads.
#[derive(Default)]
struct Board {
    clipboard: Offered,
    primary: Offered,
    incoming: Option<(Incoming, data::Board)>,
    /// The proof's pending sync: its id, the source it confirms, and the
    /// length it will report.
    sync: Option<(u32, u32, usize)>,
}

impl Board {
    fn offered(&mut self, board: data::Board) -> &mut Offered {
        match board {
            data::Board::Clipboard => &mut self.clipboard,
            data::Board::Primary => &mut self.primary,
        }
    }

    /// Drops the paste in flight if it reads `board`'s selection.
    fn cancel_paste(&mut self, board: data::Board) {
        if self
            .incoming
            .as_ref()
            .is_some_and(|(_, from)| *from == board)
        {
            self.incoming = None;
        }
    }
}

/// A selection's name in what td-term says on stderr.
fn named(board: data::Board) -> &'static str {
    match board {
        data::Board::Clipboard => "clipboard",
        data::Board::Primary => "primary selection",
    }
}

/// The pointer selection being made: the anchor and extent a frame closes.
#[derive(Default)]
struct Drag {
    position: (i32, i32),
    /// The pointer is over the surface, between its enter and leave.
    inside: bool,
    left_down: bool,
    anchor: Option<(i32, i32)>,
    extent: Option<(i32, i32)>,
    /// The link under a Control-press, read from the screen as it stood at
    /// the press, which the frame closing the press follows.
    link: Option<String>,
    /// A followed press is still held: its drag and release select nothing.
    following: bool,
    /// What the press selects by, from how many came at its cell.
    unit: render::Unit,
    /// The last press: when, at which cell, and its count.
    last_press: Option<(u64, (usize, usize), u8)>,
    /// The drag has left its press's cell; until then a plain press
    /// selects nothing.
    moved: bool,
    /// The serial of the release the next frame closes, at which the
    /// selection becomes the primary selection.
    released: Option<u32>,
}

/// A scrollback search under way: its query; the match it shows and the
/// start of the last it showed, both in the numbering of one history
/// epoch on one screen; and what the view and selection were, put back if
/// it is cancelled.
struct Search {
    query: String,
    found: Option<td_ui::vt::Found>,
    last: Option<td_ui::vt::Place>,
    epoch: u64,
    alternate: bool,
    viewport: keys::Viewport,
    selection: Option<render::Selection>,
}

/// The pointer as the child sees it while it asks for reports: which
/// buttons it was told were pressed, so their releases go to it too; the
/// cell the last press or motion report named; and the wheel rows not yet
/// a whole notch.
#[derive(Default)]
struct Reported {
    held: [bool; 3],
    cell: Option<(usize, usize)>,
    wheel: isize,
}

/// A button the child can be told about, and its slot in `Reported::held`.
fn reported_button(button: u32) -> Option<(keys::Button, usize)> {
    match button {
        LEFT_BUTTON => Some((keys::Button::Left, 0)),
        MIDDLE_BUTTON => Some((keys::Button::Middle, 1)),
        RIGHT_BUTTON => Some((keys::Button::Right, 2)),
        _ => None,
    }
}

/// The clipboard proof's markers, each said once per condition.
#[derive(Default)]
struct Proof {
    enabled: bool,
    focus: Option<u32>,
    target: Option<(u16, u16, u16, u16)>,
    selection: bool,
}

pub struct Window {
    client: Client<Object>,
    font: td_ui::font::Font,
    /// The cell the grid is laid on, the outline face's when there is one
    /// (`vt_render::cell_size`).
    cell: (usize, usize),
    palette: render::Palette,
    fallback: Size,
    clock: u64,
    options: Options,
    wayland_display: String,
    pty: Pty,
    input: Arc<pty::Input>,
    model: Option<Terminal>,
    events: Receiver<Event>,
    sender: Option<SyncSender<Event>>,
    waker: Waker,
    child: Option<Child>,
    /// The size the surface holds, from the last acknowledged configure.
    current: Option<Size>,
    /// Whether any configure CHOSE a size; the first on td's compositor is
    /// zero in both axes, and a terminal that stopped there would announce
    /// its own fallback.
    layout_configured: bool,
    /// An acknowledged configure no frame has committed yet.
    needs_commit: bool,
    drawn: Option<Drawn>,
    frame: Option<Frame>,
    /// Until when frames ring the visual bell: set when a frame that took
    /// the model's bell is submitted, and put forward by every later one.
    flash: Option<u64>,
    /// The grid the PTY was last set to and verified at.
    cells: Option<(u16, u16)>,
    /// The surface size the PTY and the model were last adopted for, which a
    /// frame that failed to present leaves ahead of `drawn`.
    adopted: Option<Size>,
    /// The last drain stopped at its bound with events still queued.
    backlog: bool,
    /// The model or the view changed under a picture that did not.
    stale: bool,
    keymap_ready: bool,
    viewport: keys::Viewport,
    selection: Option<render::Selection>,
    drag: Drag,
    search: Option<Search>,
    reported: Reported,
    wheel: Wheel,
    board: Board,
    proof: Proof,
    /// The outline face the cells are drawn in, fitted to the bitmap face's
    /// cell or at a size of its own, with its zoom; `None` draws every cell
    /// from the bitmap face.
    outline: Option<render::Zoom>,
    /// The browser command a link opens with; none, as in production, is
    /// `BROWSER` then `xdg-open` (td-ui's opener).
    browser: Option<String>,
    /// The desktop profile's exit status, once the loop is closed.
    exit: u8,
}

impl Window {
    fn new(
        stream: std::os::unix::net::UnixStream,
        options: Options,
        display: String,
    ) -> Result<Self> {
        let font = td_ui::font::pinned()?;
        let cell = render::cell_size(&font, None);
        let fallback = default_size(cell)?;
        let mut client = Client::new(stream, std::env::temp_dir())?;
        client.want_primary();
        let waker = client.connection().waker()?;
        // Before the first frame: a machine whose devpts is missing should
        // fail without having drawn a window.
        let pty = Pty::open()?;
        let (sender, events) = sync_channel(MAX_PENDING_EVENTS);
        Ok(Self {
            client,
            font,
            cell,
            palette: render::Palette::pinned(),
            fallback,
            clock: 0,
            options,
            wayland_display: display,
            pty,
            // Carried from startup: keys struck between the surface mapping
            // and the child existing are type-ahead the writer will drain.
            input: pty::Input::new(),
            model: None,
            events,
            sender: Some(sender),
            waker,
            child: None,
            current: None,
            layout_configured: false,
            needs_commit: false,
            drawn: None,
            frame: None,
            flash: None,
            cells: None,
            adopted: None,
            backlog: false,
            stale: false,
            keymap_ready: false,
            viewport: keys::Viewport::new(),
            selection: None,
            drag: Drag::default(),
            search: None,
            reported: Reported::default(),
            wheel: Wheel::default(),
            board: Board::default(),
            proof: Proof::default(),
            outline: None,
            browser: None,
            exit: 0,
        })
    }

    /// Draws in `outline`, sized at startup by `start`: the grid is laid on
    /// its cell, and the fallback grid is that cell's.
    fn set_outline(&mut self, outline: Option<Face>, start: Sizing) -> Result<()> {
        self.cell = render::cell_size(&self.font, outline.as_ref());
        self.fallback = default_size(self.cell)?;
        self.outline = outline.map(|face| render::Zoom::new(face, start));
        Ok(())
    }

    /// Moves the outline face `to`. A new size's cell lays another grid on
    /// the same surface, which the next draw adopts as it adopts a resize,
    /// so the PTY learns the grid before the pixels move. With no outline
    /// face there is no size to change.
    fn zoom(&mut self, to: render::ZoomTo) {
        let Some(outline) = self.outline.as_mut() else {
            return;
        };
        if outline.zoom(to) {
            self.cell = render::cell_size(&self.font, Some(outline.face()));
            self.adopted = None;
            self.stale = true;
        }
    }

    fn modes(&self) -> keys::Modes {
        keys::Modes {
            application_cursor: self
                .model
                .as_ref()
                .and_then(|terminal| terminal.mode("application-cursor"))
                .unwrap_or(false),
        }
    }

    fn history(&self) -> keys::Scrollback {
        self.model
            .as_ref()
            .map(Terminal::scrollback)
            .unwrap_or_default()
    }

    fn ring(&mut self) {
        if let Some(terminal) = self.model.as_mut() {
            terminal.ring();
            self.stale = true;
        }
    }

    /// Queue bytes for the child whole, or ring: a queue that cannot take a
    /// whole sequence means the child is not draining.
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        if !self.input.push(bytes)? {
            self.ring();
        }
        Ok(())
    }

    fn clear_selection(&mut self) {
        if self.selection.take().is_some() {
            self.stale = true;
        }
    }

    /// Move the view, and ask for a frame only if it actually moved.
    fn scroll(&mut self, action: &keys::Action) {
        let rows = self.cells.map_or(0, |(rows, _)| usize::from(rows));
        let history = self.history();
        let before = self.viewport.offset(history);
        self.viewport.apply(action, rows, history);
        if self.viewport.offset(history) != before {
            self.stale = true;
            self.clear_selection();
        }
    }

    /// One chord, routed: to the viewport, to the child, or nowhere.
    /// Returns whether it did anything, which is what a repeat asks.
    fn route(&mut self, chord: &str, text: Option<char>) -> Result<bool> {
        let viewing = self.viewport.viewing(self.history());
        let action = keys::action(chord, text, self.modes(), viewing);
        match action {
            keys::Action::Bytes(sequence) => {
                self.send(sequence.as_slice())?;
                // Ordinary input returns to the live bottom, so typing never
                // echoes where nobody can see it.
                self.scroll(&action);
                Ok(true)
            }
            keys::Action::Scroll(_) => {
                self.scroll(&action);
                Ok(true)
            }
            keys::Action::Silent => Ok(false),
        }
    }

    fn key(&mut self, serial: u32, key: u32, stroke: &Stroke) -> Result<()> {
        let chord = stroke.chord.as_str();
        if self.search.is_some() {
            return self.search_key(serial, key, stroke);
        }
        if chord == SEARCH_CHORD {
            self.search = Some(Search {
                query: String::new(),
                found: None,
                last: None,
                epoch: self.history().epoch,
                alternate: self.alternate(),
                viewport: self.viewport,
                selection: self.selection.take(),
            });
            self.stale = true;
            return Ok(());
        }
        if chord == COPY_CHORD {
            return self.copy(serial);
        }
        if chord == PASTE_CHORD {
            if !self.paste(data::Board::Clipboard)? {
                self.ring();
            }
            return Ok(());
        }
        // The font chords are td-term's whether or not there is a face to
        // zoom: the child's table sends nothing for them.
        if let Some(to) = zoom_to(chord) {
            self.zoom(to);
            return Ok(());
        }
        self.clear_selection();
        if self.route(chord, stroke.text)? && stroke.repeat && !self.proof.enabled {
            self.client.arm(key, self.clock);
        }
        Ok(())
    }

    fn alternate(&self) -> bool {
        self.model
            .as_ref()
            .is_some_and(|model| model.mode("alternate-screen") == Some(true))
    }

    /// Drops a match the text under it no longer holds: a clear has
    /// renumbered history, the other screen has come up, or output has
    /// rewritten its cells. Its last place goes with a renumbering or a
    /// screen change, since it names a line of what went.
    fn check_search(&mut self) {
        let alternate = self.alternate();
        let epoch = self.history().epoch;
        let (Some(search), Some(terminal)) = (self.search.as_mut(), self.model.as_ref()) else {
            return;
        };
        if search.epoch != epoch || search.alternate != alternate {
            search.epoch = epoch;
            search.alternate = alternate;
            search.found = None;
            search.last = None;
        }
        if let Some(found) = search.found {
            if !terminal.still_matches(&search.query, found) {
                search.found = None;
            }
        }
    }

    /// One key while a search is open, which takes them all: text extends
    /// the query, Backspace shortens it, the search chords step to an older
    /// or newer match, Return ends the search with the match selected and
    /// made the primary selection, and Escape (or C-g, C-c) puts the view
    /// and selection back as they were. Nothing reaches the child. A key
    /// that edits or steps repeats while held, as foot's do, its repeats
    /// coming back here through `end_turn`, not to the child.
    fn search_key(&mut self, serial: u32, key: u32, stroke: &Stroke) -> Result<()> {
        let chord = stroke.chord.as_str();
        self.check_search();
        self.stale = true;
        if SEARCH_CANCEL_CHORDS.contains(&chord) {
            if let Some(search) = self.search.take() {
                self.viewport = search.viewport;
                self.selection = search.selection;
            }
            return Ok(());
        }
        if chord == SEARCH_COMMIT_CHORD {
            let Some(search) = self.search.take() else {
                return Ok(());
            };
            let Some(found) = search.found else {
                // Nothing to select: the selection is as it was.
                self.selection = search.selection;
                return Ok(());
            };
            self.show_match(found);
            self.search = Some(search);
            self.selection = self.search_selection();
            self.search = None;
            if self.selection.is_some() {
                self.own_primary(serial)?;
            }
            return Ok(());
        }
        if self.search_stroke(stroke) && stroke.repeat && !self.proof.enabled {
            self.client.arm(key, self.clock);
        }
        Ok(())
    }

    /// A stroke that edits the query or steps between matches, pressed or
    /// repeated. Returns whether it did either, which is what a repeat
    /// asks; a ring, at a bound or a dead end, stops one.
    fn search_stroke(&mut self, stroke: &Stroke) -> bool {
        let chord = stroke.chord.as_str();
        let Some(search) = self.search.as_mut() else {
            return false;
        };
        let shown = search.found.map(|found| found.start).or(search.last);
        let stepped = SEARCH_OLDER_CHORDS.contains(&chord) || SEARCH_NEWER_CHORDS.contains(&chord);
        let (from, toward) = if SEARCH_OLDER_CHORDS.contains(&chord) {
            (shown, td_ui::vt::Toward::Older)
        } else if SEARCH_NEWER_CHORDS.contains(&chord) {
            (shown, td_ui::vt::Toward::Newer)
        } else {
            if chord == SEARCH_ERASE_CHORD {
                search.query.pop();
            } else if let Some(text) = stroke
                .text
                .filter(|_| !chord.starts_with("C-") && !chord.starts_with("M-"))
                .filter(|text| !text.is_control())
            {
                if search.query.chars().count() >= td_ui::vt::MAX_QUERY {
                    self.ring();
                    return false;
                }
                search.query.push(text);
            } else {
                return false;
            }
            // A changed query looks again from the match shown, or the last
            // one, which stays if it still matches.
            let from = shown.map(|(line, column)| (line, column.saturating_add(1)));
            (from, td_ui::vt::Toward::Older)
        };
        let Some(terminal) = self.model.as_ref() else {
            return false;
        };
        let found = terminal.search(&search.query, from, toward);
        if found.is_none() && stepped && search.found.is_some() {
            // No further match that way: the one shown stays.
            self.ring();
            return false;
        }
        search.found = found;
        if let Some(found) = found {
            search.last = Some(found.start);
            self.show_match(found);
        }
        true
    }

    /// The first row of the view, in the search's numbering.
    fn view_top(&self) -> Option<u64> {
        let history = self.model.as_ref()?.scrollback();
        let offset = u64::try_from(self.viewport.offset(history)).ok()?;
        history.pushed.checked_sub(offset)
    }

    /// Scrolls the view so the whole of `found` is on it, its first row
    /// about mid-view, unless it already is. On the alternate screen the
    /// view returns to the live screen, the only one that shows it.
    fn show_match(&mut self, found: td_ui::vt::Found) {
        let alternate = self.alternate();
        let Some(terminal) = self.model.as_ref() else {
            return;
        };
        let history = terminal.scrollback();
        if alternate {
            self.viewport
                .apply(&keys::Action::Scroll(keys::Scroll::Bottom), 0, history);
            return;
        }
        let rows = u64::try_from(terminal.rows()).unwrap_or(u64::MAX);
        let current = self.viewport.offset(history);
        let shown = |offset: usize| {
            let top = history
                .pushed
                .saturating_sub(u64::try_from(offset).unwrap_or(u64::MAX));
            found.start.0 >= top && found.end.0 < top.saturating_add(rows)
        };
        if shown(current) {
            return;
        }
        // Mid-view, or as low as lets the match's last row on.
        let span = found.end.0.saturating_sub(found.start.0);
        let row = (rows / 2).min(rows.saturating_sub(1).saturating_sub(span));
        let target = history
            .pushed
            .saturating_add(row)
            .saturating_sub(found.start.0);
        let target = usize::try_from(target)
            .unwrap_or(usize::MAX)
            .min(history.lines);
        let back =
            i64::try_from(target).unwrap_or(i64::MAX) - i64::try_from(current).unwrap_or(i64::MAX);
        self.viewport
            .by_lines(i32::try_from(back).unwrap_or(0), history);
    }

    /// The search's match as a selection of the view, cut to the part of
    /// it on the view, if any is. A match on the alternate screen is on
    /// the live view only: scrolled back, the view shows the primary's
    /// history.
    fn search_selection(&self) -> Option<render::Selection> {
        let found = self.search.as_ref()?.found?;
        let terminal = self.model.as_ref()?;
        if self.alternate() && self.viewport.offset(terminal.scrollback()) != 0 {
            return None;
        }
        let top = self.view_top()?;
        let rows = u64::try_from(terminal.rows()).ok()?;
        let bottom = top.checked_add(rows)?.checked_sub(1)?;
        if found.end.0 < top || found.start.0 > bottom {
            return None;
        }
        let row = |line: u64| usize::try_from(line.saturating_sub(top)).ok();
        let anchor = if found.start.0 < top {
            (0, 0)
        } else {
            (row(found.start.0)?, found.start.1)
        };
        let extent = if found.end.0 > bottom {
            (row(bottom)?, terminal.columns().saturating_sub(1))
        } else {
            (row(found.end.0)?, found.end.1)
        };
        Some(render::Selection { anchor, extent })
    }

    /// The search line, and the edge it covers: the last row, unless the
    /// match is on it, then the first.
    fn search_status(&self) -> Option<(String, render::Edge)> {
        let search = self.search.as_ref()?;
        let missing = !search.query.is_empty() && search.found.is_none();
        let label = if missing {
            "search (no match)"
        } else {
            "search"
        };
        let last = self.model.as_ref()?.rows().saturating_sub(1);
        let edge = match self.search_selection() {
            Some(selection) if selection.anchor.0.max(selection.extent.0) == last => {
                render::Edge::Top
            }
            _ => render::Edge::Bottom,
        };
        Some((format!("{label}: {}", search.query), edge))
    }

    /// What the frame shows as selected: the search's match while one is
    /// open, else td-term's selection.
    fn shown_selection(&self) -> Option<render::Selection> {
        if self.search.is_some() {
            self.search_selection()
        } else {
            self.selection
        }
    }

    /// The pointer reporting the child asked for, whatever the view.
    fn asked_mouse_mode(&self) -> Option<td_ui::vt::MouseMode> {
        let mode = self.model.as_ref()?.mouse();
        (mode.tracking != td_ui::vt::MouseTracking::Off).then_some(mode)
    }

    /// The pointer reporting the child asked for, while the view is the
    /// live screen the child's cells are on.
    fn mouse_mode(&self) -> Option<td_ui::vt::MouseMode> {
        let live = !self.viewport.viewing(self.history());
        self.asked_mouse_mode().filter(|_| live)
    }

    fn pointer_modifiers(&self) -> keys::PointerModifiers {
        let held = self.client.input().held();
        keys::PointerModifiers {
            shift: held.shift,
            alt: held.alt,
            control: held.control,
        }
    }

    /// `event`'s report at `cell` under `mode`, if the mode has one.
    fn report_at(
        &self,
        event: keys::Pointer,
        cell: (usize, usize),
        mode: td_ui::vt::MouseMode,
    ) -> Option<keys::Report> {
        keys::report(event, cell, self.pointer_modifiers(), mode)
    }

    /// The lowest-numbered button the child was told is down, as xterm
    /// names the button a motion report carries.
    fn reported_held(&self) -> Option<keys::Button> {
        [
            keys::Button::Left,
            keys::Button::Middle,
            keys::Button::Right,
        ]
        .into_iter()
        .zip(self.reported.held)
        .find_map(|(button, held)| held.then_some(button))
    }

    /// Releases every button the child was told is down, at the last cell
    /// it was told of, unless it has since stopped asking; then forgets
    /// them. A Leave does this, so a button whose release the compositor
    /// sends elsewhere is not left down for the child, nor its next motion
    /// taken for a drag.
    fn release_reported(&mut self) -> Result<()> {
        let held = std::mem::take(&mut self.reported.held);
        let cell = self.reported.cell.take();
        self.reported.wheel = 0;
        let (Some(mode), Some(cell)) = (self.asked_mouse_mode(), cell) else {
            return Ok(());
        };
        let buttons = [
            keys::Button::Left,
            keys::Button::Middle,
            keys::Button::Right,
        ];
        for (button, held) in buttons.into_iter().zip(held) {
            if let Some(report) = held
                .then(|| self.report_at(keys::Pointer::Release(button), cell, mode))
                .flatten()
            {
                self.send(report.as_slice())?;
            }
        }
        Ok(())
    }

    /// The child's half of the pointer, and whether it took `event`. A
    /// press is the child's while it asks for reports at the live screen,
    /// unless Shift is held, which keeps the gesture td-term's, or
    /// td-term's own drag is under way. A press the child took is its
    /// gesture to the end: its release is the child's whatever is held or
    /// shown by then, absorbed if the child has since stopped asking, and
    /// motion while it is held is reported at the live screen under 1002
    /// and 1003. Buttonless motion is reported under 1003 alone, not under
    /// Shift. No motion is reported during td-term's own drag, and each
    /// report names a cell once. Motion still moves the pointer td-term
    /// tracks, and a Leave goes on to td-term's handler too.
    fn report_pointer(&mut self, event: &pointer::Event) -> Result<bool> {
        match *event {
            pointer::Event::Button {
                button, pressed, ..
            } => {
                let Some((button, slot)) = reported_button(button) else {
                    return Ok(false);
                };
                if !pressed {
                    let Some(held) = self.reported.held.get_mut(slot).filter(|held| **held) else {
                        return Ok(false);
                    };
                    *held = false;
                    let release = self
                        .asked_mouse_mode()
                        .zip(self.cell_at(self.drag.position))
                        .and_then(|(mode, cell)| {
                            self.report_at(keys::Pointer::Release(button), cell, mode)
                        });
                    if let Some(report) = release {
                        self.send(report.as_slice())?;
                    }
                    return Ok(true);
                }
                let (Some(mode), Some(cell)) =
                    (self.mouse_mode(), self.cell_at(self.drag.position))
                else {
                    return Ok(false);
                };
                if self.pointer_modifiers().shift || self.drag.left_down {
                    return Ok(false);
                }
                if let Some(held) = self.reported.held.get_mut(slot) {
                    *held = true;
                }
                self.reported.cell = Some(cell);
                self.drag.last_press = None;
                if let Some(report) = self.report_at(keys::Pointer::Press(button), cell, mode) {
                    self.send(report.as_slice())?;
                }
                Ok(true)
            }
            pointer::Event::Motion(x, y) => {
                self.drag.position = (x, y);
                let (Some(mode), Some(cell)) = (self.mouse_mode(), self.cell_at((x, y))) else {
                    return Ok(false);
                };
                let held = self.reported_held();
                if self.drag.left_down
                    || (held.is_none() && self.pointer_modifiers().shift)
                    || Some(cell) == self.reported.cell
                {
                    return Ok(false);
                }
                if let Some(report) = self.report_at(keys::Pointer::Motion(held), cell, mode) {
                    self.reported.cell = Some(cell);
                    // Motion is not worth a bell: a child that is not
                    // reading would ring it on every cell the pointer
                    // crossed.
                    self.input.push(report.as_slice())?;
                }
                Ok(false)
            }
            pointer::Event::Leave(_) => {
                self.release_reported()?;
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    /// A frame's wheel rows as the child's wheel presses, one for every
    /// three rows, the remainder carried to the next frame so a smooth
    /// wheel reports at a notched one's rate, at most `MAX_WHEEL_REPORTS`
    /// a frame. The wheel is td-term's under Shift, while the view is
    /// scrolled back, and under mode 9, which reports presses alone.
    fn report_wheel(&mut self, rows: isize) -> Result<bool> {
        let taken = self
            .mouse_mode()
            .zip(self.cell_at(self.drag.position))
            .filter(|(mode, _)| {
                !self.pointer_modifiers().shift && mode.tracking != td_ui::vt::MouseTracking::Press
            });
        // A wheel td-term keeps ends the child's: its remainder is not
        // carried into the next notch the child hears.
        let Some((mode, cell)) = taken else {
            self.reported.wheel = 0;
            return Ok(false);
        };
        if self.reported.wheel.signum() != rows.signum() {
            self.reported.wheel = 0;
        }
        self.reported.wheel = self.reported.wheel.saturating_add(rows);
        let notches = self.reported.wheel / 3;
        self.reported.wheel %= 3;
        let button = if notches < 0 {
            keys::Button::WheelUp
        } else {
            keys::Button::WheelDown
        };
        let Some(report) = self.report_at(keys::Pointer::Press(button), cell, mode) else {
            return Ok(true);
        };
        for _ in 0..notches.unsigned_abs().min(MAX_WHEEL_REPORTS) {
            self.send(report.as_slice())?;
        }
        Ok(true)
    }

    fn pointer(&mut self, event: pointer::Event) -> Result<()> {
        // A press ends a search, leaving the view where it is, so the press
        // acts on what it was made over.
        if matches!(event, pointer::Event::Button { pressed: true, .. }) {
            if let Some(search) = self.search.take() {
                self.selection = search.selection;
                self.stale = true;
            }
        }
        if self.report_pointer(&event)? {
            return Ok(());
        }
        match event {
            pointer::Event::Enter { x, y, .. } => {
                self.drag.position = (x, y);
                self.drag.inside = true;
            }
            pointer::Event::Motion(x, y) => {
                self.drag.position = (x, y);
                if self.drag.left_down {
                    self.drag.extent = Some((x, y));
                }
            }
            pointer::Event::Leave(_) => {
                // A drag released outside the surface ends release, leave,
                // frame: the frame still finishes it. One held is abandoned.
                if self.drag.left_down || self.drag.released.is_none() {
                    self.drag.anchor = None;
                    self.drag.extent = None;
                    self.drag.released = None;
                }
                self.drag.left_down = false;
                self.drag.last_press = None;
                self.drag.inside = false;
            }
            pointer::Event::Button {
                button: LEFT_BUTTON,
                pressed,
                serial,
            } => {
                self.drag.left_down = pressed;
                if pressed {
                    let held = self.client.input().held();
                    self.drag.following = false;
                    self.drag.link = (held.control && !held.alt && !held.shift)
                        .then(|| self.shown_link(self.drag.position))
                        .flatten();
                    self.drag.anchor = Some(self.drag.position);
                    self.drag.unit = self.count_press();
                    self.drag.moved = false;
                    self.drag.released = None;
                } else if std::mem::take(&mut self.drag.following) {
                    return Ok(());
                } else {
                    self.drag.released = Some(serial);
                }
                self.drag.extent = Some(self.drag.position);
            }
            // A middle press pastes the primary selection; with nothing
            // to paste it does nothing, as foot's does.
            pointer::Event::Button {
                button: MIDDLE_BUTTON,
                pressed: true,
                ..
            } => {
                self.drag.last_press = None;
                self.paste(data::Board::Primary)?;
            }
            // Another button's press ends a run of left presses.
            pointer::Event::Button { pressed: true, .. } => self.drag.last_press = None,
            pointer::Event::Button { .. } => {}
            // A frame is the transaction: the selection and the wheel are
            // applied once when it closes.
            pointer::Event::Frame => {
                if let Some(link) = self.drag.link.take() {
                    self.drag.anchor = None;
                    self.drag.extent = None;
                    self.drag.released = None;
                    // A followed press counts toward no word or row.
                    self.drag.last_press = None;
                    // A release in the same frame already came and went.
                    self.drag.following = std::mem::take(&mut self.drag.left_down);
                    self.follow(&link);
                } else if let Some(extent) = self.drag.extent.take() {
                    // The anchor lasts the whole drag, so output that clears
                    // the selection mid-drag cannot strand the rest of it.
                    let anchor = if self.drag.left_down {
                        self.drag.anchor
                    } else {
                        self.drag.anchor.take()
                    };
                    self.select(anchor, extent);
                    if !self.drag.left_down {
                        if let Some(serial) = self.drag.released.take() {
                            self.own_primary(serial)?;
                        }
                    }
                }
                let (rows, _) = self.wheel.frame();
                if rows != 0 && !self.report_wheel(rows)? {
                    // Positive is toward the live bottom, so it scrolls
                    // forward; a wheel turned away goes back into history.
                    let lines = i32::try_from(rows.saturating_neg()).unwrap_or(if rows < 0 {
                        i32::MAX
                    } else {
                        i32::MIN
                    });
                    let history = self.history();
                    let before = self.viewport.offset(history);
                    self.viewport.by_lines(lines, history);
                    if self.viewport.offset(history) != before {
                        self.stale = true;
                        self.clear_selection();
                    }
                }
            }
            _ => self.wheel.update(event)?,
        }
        Ok(())
    }

    /// The link under a Control-press, from the screen as the person saw
    /// it: a model changed since the last committed frame (`stale`, which a
    /// resize's reflow sets), or a committed frame for a changed model whose
    /// callback has not said it reached the screen (`shows_model`), is not
    /// what was on screen, and the press is then a plain one.
    fn shown_link(&self, fixed: (i32, i32)) -> Option<String> {
        let shown = !self.stale && self.frame.as_ref().is_some_and(|frame| frame.shows_model);
        shown.then(|| self.link_at(fixed)).flatten()
    }

    fn link_at(&self, fixed: (i32, i32)) -> Option<String> {
        self.link_span(fixed).map(|(_, link)| link)
    }

    /// The link Control-hover rules: the one a Control-press at the
    /// pointer would follow, while the pointer is over the surface and
    /// Control alone is held (Caps and Num Lock aside). None while the
    /// child takes the press as a report, or a search the press would
    /// end is open.
    fn hovered_link(&self) -> Option<render::LinkSpan> {
        let held = self.client.input().held();
        if !self.drag.inside || !held.control || held.alt || held.shift {
            return None;
        }
        if self.mouse_mode().is_some() || self.search.is_some() {
            return None;
        }
        self.link_span(self.drag.position).map(|(span, _)| span)
    }

    /// The link under a pointer position in the viewport's row, as td-ui's
    /// rule finds it, and the cells it covers; none past the drawn grid,
    /// whose edge cells a selection's clamp would reach. A cell holds one
    /// scalar, so the row's text is its cells in order; a link the terminal
    /// wrapped onto the next row is found only up to the row's end.
    fn link_span(&self, fixed: (i32, i32)) -> Option<(render::LinkSpan, String)> {
        let (rows, columns) = self.cells?;
        let inside = |value: i32, cells: u16, cell: usize| {
            usize::try_from(value.div_euclid(256))
                .is_ok_and(|pixel| pixel < usize::from(cells).saturating_mul(cell))
        };
        if !inside(fixed.0, columns, self.cell.0) || !inside(fixed.1, rows, self.cell.1) {
            return None;
        }
        let (row, column) = self.cell_at(fixed)?;
        let terminal = self.model.as_ref()?;
        let viewport = self.viewport.offset(terminal.scrollback());
        let snapshot = render::Snapshot::new(terminal, false, false).scrolled_back(viewport);
        let mut text = String::new();
        // Where each cell's scalar starts in the text.
        let mut starts = Vec::new();
        // Only the drawn columns: a model wider than the surface is clipped.
        for cell in 0..snapshot.columns().min(usize::from(columns)) {
            starts.push(text.len());
            text.push(snapshot.cell(row, cell).scalar);
        }
        let range = td_ui::links::at(&text, *starts.get(column)?)?;
        let start = starts.iter().position(|&at| at == range.start)?;
        let end = starts
            .iter()
            .position(|&at| at >= range.end)
            .unwrap_or(starts.len());
        let link = text.get(range)?.to_owned();
        Some((render::LinkSpan { row, start, end }, link))
    }

    /// Opens a followed link on the display the terminal is on; one that
    /// cannot be opened is a line on stderr and the bell.
    fn follow(&mut self, link: &str) {
        let display = Path::new(&self.wayland_display);
        if let Err(error) = td_ui::open::link_on(link, self.browser.as_deref(), Some(display)) {
            let _ = writeln!(std::io::stderr().lock(), "td-term: open link: {error}");
            self.ring();
        }
    }

    fn cell_at(&self, fixed: (i32, i32)) -> Option<(usize, usize)> {
        let (rows, columns) = self.cells?;
        let (width, height) = self.cell;
        if rows == 0 || columns == 0 || width == 0 || height == 0 {
            return None;
        }
        let pixel = |value: i32| usize::try_from(value.div_euclid(256)).unwrap_or(0);
        let row = (pixel(fixed.1) / height).min(usize::from(rows).saturating_sub(1));
        let column = (pixel(fixed.0) / width).min(usize::from(columns).saturating_sub(1));
        Some((row, column))
    }

    /// The press's unit from the presses that came at its cell in quick
    /// succession: a second selects the word, a third the row, and a
    /// fourth starts over at a cell.
    fn count_press(&mut self) -> render::Unit {
        let cell = self.cell_at(self.drag.position);
        let count = match (self.drag.last_press, cell) {
            (Some((at, previous, count)), Some(cell))
                if previous == cell && self.clock.saturating_sub(at) <= MULTI_CLICK_MS =>
            {
                count % 3 + 1
            }
            _ => 1,
        };
        self.drag.last_press = cell.map(|cell| (self.clock, cell, count));
        match count {
            2 => render::Unit::Word,
            3 => render::Unit::Row,
            _ => render::Unit::Cell,
        }
    }

    /// Selects from the press's anchor to `extent` by the press's unit. A
    /// plain press selects nothing until its drag leaves its cell, so a
    /// click clears the selection rather than taking one cell.
    fn select(&mut self, anchor: Option<(i32, i32)>, extent: (i32, i32)) {
        let Some(extent) = self.cell_at(extent) else {
            return;
        };
        let pressed = anchor.and_then(|anchor| self.cell_at(anchor));
        let (anchor, unit) = match (pressed, self.selection) {
            (Some(anchor), _) => (anchor, self.drag.unit),
            (None, Some(selection)) => (selection.anchor, render::Unit::Cell),
            (None, None) => return,
        };
        let unmoved =
            pressed.is_some() && unit == render::Unit::Cell && !self.drag.moved && extent == anchor;
        let next = if unmoved {
            None
        } else {
            let Some(terminal) = self.model.as_ref() else {
                return;
            };
            let viewport = self.viewport.offset(terminal.scrollback());
            let snapshot = render::Snapshot::new(terminal, false, false).scrolled_back(viewport);
            Some(snapshot.select(unit, anchor, extent))
        };
        if pressed.is_some() && !unmoved {
            self.drag.moved = true;
        }
        if next != self.selection {
            self.selection = next;
            self.stale = true;
        }
    }

    /// The selection's text, bounded by the clipboard's ceiling: a row the
    /// terminal wrapped runs on into the next whole, as the child wrote
    /// it; any other ends in a newline with its trailing blanks dropped.
    fn selected_text(&self) -> Result<Option<String>> {
        let (Some(selection), Some(terminal)) = (self.selection, self.model.as_ref()) else {
            return Ok(None);
        };
        let (start, end) = if selection.anchor <= selection.extent {
            (selection.anchor, selection.extent)
        } else {
            (selection.extent, selection.anchor)
        };
        let viewport = self.viewport.offset(terminal.scrollback());
        let snapshot =
            render::Snapshot::new(terminal, self.client.activated(), false).scrolled_back(viewport);
        let mut selected = String::new();
        for row in start.0..=end.0 {
            let first = if row == start.0 { start.1 } else { 0 };
            let last = if row == end.0 {
                end.1
            } else {
                snapshot.columns().saturating_sub(1)
            };
            let line_start = selected.len();
            for column in first..=last {
                selected.push(snapshot.cell(row, column).scalar);
            }
            let joined = row != end.0 && snapshot.wrapped(row);
            // Bounded after the trim: the ceiling is on what is copied, and
            // one row past it is all the untrimmed text can overshoot by.
            while !joined && selected.len() > line_start && selected.ends_with(' ') {
                selected.pop();
            }
            if selected.len() > MAX_CLIPBOARD_BYTES {
                return Err(format!(
                    "terminal selection exceeds {MAX_CLIPBOARD_BYTES} bytes"
                ));
            }
            if row != end.0 && !joined {
                selected.push('\n');
            }
        }
        // A wrapped row's edge blanks are kept for the row that follows;
        // with nothing after them they end the text, and go.
        let kept = selected.trim_end_matches(' ').len();
        selected.truncate(kept);
        Ok((!selected.is_empty()).then_some(selected))
    }

    /// Offer the selection on the clipboard at the press's serial. A copy
    /// with nothing selected does nothing; one the clipboard cannot take
    /// rings.
    fn copy(&mut self, serial: u32) -> Result<()> {
        let text = match self.selected_text() {
            Ok(Some(text)) => text,
            Ok(None) => return Ok(()),
            Err(error) => {
                let _ = writeln!(std::io::stderr().lock(), "td-term: copy selection: {error}");
                self.ring();
                return Ok(());
            }
        };
        if !self.client.clipboard() || !self.client.input().focused {
            self.ring();
            return Ok(());
        }
        let source = self.client.offer_selection(serial)?;
        let length = text.len();
        self.board.clipboard.text = Some(Arc::from(text));
        if self
            .board
            .incoming
            .as_ref()
            .is_some_and(|(_, from)| *from == data::Board::Clipboard)
        {
            self.board.incoming = None;
            self.stale = true;
        }
        if self.proof.enabled {
            // The compositor answers a sync after the selection request, so
            // its callback is when the selection is the compositor's.
            let id = self.client.allocate(Object::Sync)?;
            self.client.words(DISPLAY, 0, &[id])?;
            self.board.sync = Some((id, source, length));
        }
        Ok(())
    }

    /// Makes the selection a release finished the primary selection at
    /// the release's serial, as foot does. Quietly: a seat without one, or
    /// a selection over the bound, is nothing the person asked about.
    fn own_primary(&mut self, serial: u32) -> Result<()> {
        if !self.client.primary() {
            return Ok(());
        }
        let Ok(Some(text)) = self.selected_text() else {
            return Ok(());
        };
        self.client.offer_primary(serial)?;
        self.board.primary.text = Some(Arc::from(text));
        Ok(())
    }

    /// Starts reading `board`'s selection for the child, and says whether
    /// it could: the selection must be live and offer text, the terminal
    /// focused, and no paste already in flight. An endpoint that cannot be
    /// made rings, and counts as started.
    fn paste(&mut self, board: data::Board) -> Result<bool> {
        let offered = match board {
            data::Board::Clipboard => {
                self.client.clipboard() && self.client.selection_mime().is_some()
            }
            data::Board::Primary => self.client.primary() && self.client.primary_mime().is_some(),
        };
        if !offered || !self.client.input().focused || self.board.incoming.is_some() {
            return Ok(false);
        }
        let (incoming, peer) = match Incoming::begin(self.clock) {
            Ok(pair) => pair,
            Err(error) => {
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "td-term: paste refused: {} endpoint: {error}",
                    named(board)
                );
                self.ring();
                return Ok(true);
            }
        };
        match board {
            data::Board::Clipboard => self.client.receive(&peer)?,
            data::Board::Primary => self.client.receive_primary(&peer)?,
        }
        drop(peer);
        self.board.incoming = Some((incoming, board));
        Ok(true)
    }

    /// A whole paste reaches the child, bracketed when the child asked; one
    /// the child cannot be given rings.
    fn pasted(&mut self, text: String) -> Result<()> {
        let bracketed = self
            .model
            .as_ref()
            .and_then(|terminal| terminal.mode("bracketed-paste"))
            .unwrap_or(false);
        match paste_input(text.into_bytes(), bracketed) {
            Ok(bytes) if bytes.is_empty() => {}
            Ok(bytes) => {
                if self.input.push(&bytes)? {
                    self.viewport = keys::Viewport::new();
                    self.clear_selection();
                    self.stale = true;
                } else {
                    self.ring();
                }
            }
            Err(_) => self.ring(),
        }
        Ok(())
    }

    /// One selection's outcome: a change cancels the paste reading it, a
    /// send writes what td-term offered there, and a cancel or the
    /// manager's removal drops it.
    fn selection_event(&mut self, board: data::Board, event: ClipboardEvent) {
        let clock = self.clock;
        match event {
            ClipboardEvent::Selection => self.board.cancel_paste(board),
            ClipboardEvent::Send(right) => {
                let offered = self.board.offered(board);
                // A busy send drops exactly its right.
                if offered.outgoing.is_some() {
                    return;
                }
                let Some(text) = offered.text.clone() else {
                    return;
                };
                match Outgoing::begin(right, Arc::clone(&text), clock) {
                    Ok(transfer) => offered.outgoing = Some((transfer, text)),
                    Err(error) => {
                        let _ = writeln!(
                            std::io::stderr().lock(),
                            "td-term: {} send refused: {error}",
                            named(board)
                        );
                    }
                }
            }
            ClipboardEvent::Cancelled => self.board.offered(board).text = None,
            ClipboardEvent::Released => {
                self.board.cancel_paste(board);
                let offered = self.board.offered(board);
                if let Some((transfer, _)) = offered.outgoing.take() {
                    let _ = transfer.cancel();
                }
                offered.text = None;
            }
        }
    }

    /// Step the transfers at the turn's clock. Only an idle turn admits a
    /// paste, so a focus loss or selection change still queued is seen
    /// first; an expired transfer ends whatever the turn.
    fn transfers(&mut self, now: u64, idle: bool) -> Result<()> {
        let due = |expired: bool| idle || expired;
        if self
            .board
            .incoming
            .as_ref()
            .is_some_and(|(incoming, _)| due(incoming.expired(now)))
        {
            if let Some((mut incoming, board)) = self.board.incoming.take() {
                match incoming.step(now) {
                    Ok(false) => self.board.incoming = Some((incoming, board)),
                    Ok(true) => match incoming.finish() {
                        Ok(text) if self.client.input().focused => self.pasted(text)?,
                        Ok(_) => {}
                        Err(_) => self.ring(),
                    },
                    Err(_) => self.ring(),
                }
            }
        }
        for board in [data::Board::Clipboard, data::Board::Primary] {
            let offered = self.board.offered(board);
            if !offered
                .outgoing
                .as_ref()
                .is_some_and(|(outgoing, _)| due(outgoing.expired(now)))
            {
                continue;
            }
            let Some((mut outgoing, text)) = offered.outgoing.take() else {
                continue;
            };
            match outgoing.step(now) {
                Ok(false) => offered.outgoing = Some((outgoing, text)),
                // The primary selection is read at every middle click
                // anywhere, so only the clipboard's sends are reported.
                Ok(true) if board == data::Board::Primary => {}
                Ok(true) => {
                    let mut report = format!("td-term: clipboard sent bytes={}\n", text.len());
                    if let Some(marker) = clipboard_sent_marker(self.proof.enabled, &text) {
                        report.push_str(&marker);
                    }
                    let _ = std::io::stderr().lock().write_all(report.as_bytes());
                }
                Err(error) => {
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "td-term: {} send failed: {error}",
                        named(board)
                    );
                }
            }
        }
        Ok(())
    }

    /// A flash that is over leaves the next frame to be drawn without
    /// its ring.
    fn end_flash(&mut self, now: u64) {
        if self.flash.is_some_and(|until| now >= until) {
            self.flash = None;
            self.stale = true;
        }
    }

    /// How long the next turn may wait for an event, in milliseconds, never
    /// zero: until a repeat is due, a transfer steps, the handshake expires,
    /// or at once while the child's output is still queued.
    fn next_wait(&self, now: u64) -> u64 {
        if self.backlog {
            return 1;
        }
        let mut wait = self.client.wait_ms(now);
        if self.board.incoming.is_some()
            || self.board.clipboard.outgoing.is_some()
            || self.board.primary.outgoing.is_some()
        {
            wait = wait.min(TRANSFER_WAIT_MS);
        }
        if self.child.is_none() {
            wait = wait.min(HANDSHAKE_MS.saturating_sub(now).max(1));
        }
        if let Some(until) = self.flash {
            wait = wait.min(until.saturating_sub(now));
        }
        wait.max(1)
    }

    /// What the child's threads queued since the last turn, up to the
    /// turn's bound; `backlog` says whether the next wait must be short.
    fn drain(&mut self) -> Result<()> {
        self.backlog = true;
        for _ in 0..MAX_DRAINED_PER_TURN {
            let event = match self.events.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty) => {
                    self.backlog = false;
                    return Ok(());
                }
                // The loop keeps a sender until the child starts, and both
                // producers report before they go, so a disconnect short of
                // both endings is a producer gone without reporting.
                Err(TryRecvError::Disconnected) => {
                    return match &self.child {
                        Some(child) if !child.drained || child.status.is_none() => {
                            Err("every terminal producer stopped without reporting".into())
                        }
                        _ => Ok(()),
                    }
                }
            };
            match event {
                Event::Output(bytes) => self.output(&bytes)?,
                Event::Drained => {
                    if let Some(child) = self.child.as_mut() {
                        child.drained = true;
                    }
                }
                Event::Exit(status) => {
                    if let Some(child) = self.child.as_mut() {
                        child.status = Some(status);
                    }
                }
                Event::Closed(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn output(&mut self, bytes: &[u8]) -> Result<()> {
        self.clear_selection();
        let terminal = self
            .model
            .as_mut()
            .ok_or("the child wrote before the terminal had a model")?;
        terminal.feed(bytes);
        self.check_search();
        let Some(terminal) = self.model.as_mut() else {
            return Ok(());
        };
        // Answered one at a time: the queue's unit is a reply, and one that
        // does not fit rings rather than arriving in part.
        let mut refused = false;
        for reply in terminal.take_replies() {
            refused |= !self.input.push(&reply)?;
        }
        if refused {
            terminal.ring();
        }
        self.stale = true;
        Ok(())
    }

    /// What the surface should be showing: its size, drawn with its focus.
    fn wanted(&self) -> Option<Drawn> {
        self.current.map(|size| Drawn {
            size,
            activated: self.client.activated(),
            link: self.hovered_link(),
        })
    }

    fn frame_complete(&self) -> bool {
        self.frame.as_ref().is_some_and(|frame| {
            frame.presented
                && self
                    .client
                    .buffers()
                    .iter()
                    .any(|buffer| buffer.id() == frame.buffer && !buffer.busy())
        })
    }

    /// A frame the compositor released and presented, drawn for what the
    /// surface now holds, at a size the compositor chose.
    fn ready(&self) -> bool {
        (self.layout_configured || self.options.profile == Profile::Desktop)
            && self.drawn == self.wanted()
            && self.adopted == self.current
            && self.frame_complete()
    }

    /// Readiness, and a keyboard whose keymap compiled: a shell nobody can
    /// type into is not a terminal.
    fn presented(&self) -> bool {
        self.ready() && self.client.keyboard().is_some() && self.keymap_ready
    }

    /// Whether the child may start: td's terminal once presented, its
    /// readiness; a desktop's once its first frame is committed, since a
    /// compositor may keep that buffer, or never call back for a window on
    /// another workspace, and a keymap still to come only drops keys.
    fn may_start(&self) -> bool {
        match self.options.profile {
            Profile::Td { .. } => self.presented(),
            Profile::Desktop => self.drawn.is_some() && self.cells.is_some(),
        }
    }

    /// The size changed: bound it, set the PTY and verify it took, then
    /// reflow the model — the child learns the grid before the pixels move.
    /// A grid the PTY and model already have touches neither, since a
    /// reflow resets the scrolling margins a child set.
    fn adopt(&mut self, size: Size) -> Result<()> {
        frame_bytes(size)?;
        let (rows, columns) = grid(size, self.cell)?;
        self.clear_selection();
        if self.model.is_some() && self.cells == Some((rows, columns)) {
            self.adopted = Some(size);
            self.stale = true;
            return Ok(());
        }
        let window = self.pty.resize(usize::from(rows), usize::from(columns))?;
        let (rows, columns) = (usize::from(window.rows), usize::from(window.columns));
        match self.model.as_mut() {
            // Reflowed, not rebuilt: a new model would erase the session.
            Some(terminal) => terminal.resize(rows, columns)?,
            None => self.model = Some(Terminal::new(rows, columns)?),
        }
        // The reflow may have moved a match's cells.
        self.check_search();
        self.cells = Some((window.rows, window.columns));
        self.adopted = Some(size);
        // Reflowed under the picture until a frame for it presents.
        self.stale = true;
        Ok(())
    }

    /// Start the child, and only then advertise the terminal: account,
    /// spawn and threads can each fail, and a probe told the terminal is
    /// up on one whose shell never started was told something untrue.
    fn start(&mut self) -> Result<()> {
        let (rows, columns) = self.cells.ok_or("the terminal presented without a grid")?;
        let (command, directory, environment) = match &self.options.profile {
            Profile::Td { .. } => self.td_child()?,
            Profile::Desktop => self.desktop_child()?,
        };
        if !directory.is_absolute() {
            return Err("terminal working directory is not absolute".into());
        }
        let program = launched_program_name(&self.options.command);
        let sender = self
            .sender
            .take()
            .ok_or("the terminal's child already started")?;
        let output = self
            .pty
            .master()
            .try_clone()
            .map_err(|e| format!("duplicate the terminal device for its reader: {e}"))?;
        let sink = self
            .pty
            .master()
            .try_clone()
            .map_err(|e| format!("duplicate the terminal device for its writer: {e}"))?;
        let slave = self.pty.peer()?;
        let child = pty::spawn(&command, &environment, &directory, slave)?;
        // The waiter first: it takes the child back and reaps it if its own
        // spawn fails, where a reader spawned first would leave it unwatched.
        let waker = self.waker.clone();
        let waiter = pty::spawn_waiter(child, sender.clone(), from_waited, move || {
            let _ = waker.wake();
        })?;
        let waker = self.waker.clone();
        let reader = pty::spawn_reader(output, sender, from_output, move || {
            let _ = waker.wake();
        })?;
        let writer = pty::spawn_writer(sink, Arc::clone(&self.input))?;
        let published = match &self.options.profile {
            Profile::Td { ready_socket } => Some(ready::publish(ready_socket, rows, columns)?),
            Profile::Desktop => None,
        };
        let announce = published.is_some();
        self.child = Some(Child {
            drained: false,
            status: None,
            program,
            _threads: vec![waiter, reader, writer],
            _ready: published,
        });
        if !announce {
            return Ok(());
        }
        // One locked write of one line, so a marker cannot interleave with
        // another thread's output and reach a reader as neither.
        let mut out = std::io::stdout().lock();
        out.write_all(ready::marker(rows, columns).as_bytes())
            .and_then(|()| out.flush())
            .map_err(|e| format!("write terminal ready marker: {e}"))
    }

    /// td's child: the verified account's constructed session.
    fn td_child(&self) -> Result<Launch> {
        let account = session::current_account(Path::new(PROC_STATUS), Path::new(ETC_PASSWD))?;
        let command =
            session::child_command(Path::new(session::DEFAULT_SHELL), &self.options.command)?;
        let directory = self
            .options
            .working_directory
            .clone()
            .unwrap_or_else(|| PathBuf::from(&account.home));
        let environment = session::environment(
            &account,
            std::env::var("TD_CONTROL_SOCKET").ok().as_deref(),
            &self.wayland_display,
        );
        let environment = environment
            .into_iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect();
        Ok((command, directory, environment))
    }

    /// The desktop's child: td-term's own environment, directory and shell.
    /// An entry that cannot be written is said on stderr, and the child
    /// then finds td-term's entry only where the host installed one.
    fn desktop_child(&self) -> Result<Launch> {
        let shell = session::desktop_shell(std::env::var_os("SHELL"));
        let command = session::desktop_command(shell, &self.options.command);
        let directory = match &self.options.working_directory {
            Some(directory) => directory.clone(),
            None => std::env::current_dir().map_err(|e| format!("working directory: {e}"))?,
        };
        let terminfo = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(runtime) => session::current_uid(Path::new(PROC_STATUS)).and_then(|uid| {
                let entry = td_ui::vt_terminfo::entry()?;
                session::install_runtime_terminfo(Path::new(&runtime), uid, &entry)
            }),
            None => Err("XDG_RUNTIME_DIR is not set".into()),
        };
        let terminfo = terminfo
            .map_err(|error| {
                let _ = writeln!(std::io::stderr().lock(), "td-term: terminfo: {error}");
            })
            .ok();
        let environment = session::desktop_environment(
            std::env::vars_os(),
            &self.wayland_display,
            terminfo.as_deref(),
        );
        Ok((command, directory, environment))
    }

    /// The child is gone and its output has run out: report, and end. td's
    /// profile ends in error, which its supervisor reads; the desktop
    /// closes, answering the child's status as its own.
    fn finished(&mut self) -> Result<()> {
        let Some(child) = self.child.as_ref() else {
            return Ok(());
        };
        // A desktop's window closes with its child: a background job still
        // holding the terminal keeps no window open, and no report waits on
        // the last of the output.
        let desktop = self.options.profile == Profile::Desktop;
        let Some(status) = child.status.filter(|_| child.drained || desktop) else {
            return Ok(());
        };
        if desktop {
            self.exit = exit_code(status);
            self.client.close();
            return Ok(());
        }
        if let Some(report) =
            last_screen_report(child.program.as_deref(), status, self.model.as_ref())
        {
            let _ = std::io::stderr().lock().write_all(report.as_bytes());
        }
        Err(ended(status))
    }

    fn markers(&mut self) -> Result<()> {
        if !self.proof.enabled {
            return Ok(());
        }
        let mut lines = String::new();
        if self.client.input().synchronized {
            if let Some(serial) = self.client.focus_serial() {
                if self.proof.focus != Some(serial) {
                    self.proof.focus = Some(serial);
                    lines.push_str(&format!("{CLIPBOARD_FOCUS_PREFIX}{serial}\n"));
                }
            }
        }
        if !self.stale && self.ready() {
            if let Some(line) = self.target_marker()? {
                lines.push_str(&line);
            }
            if !self.proof.selection
                && self
                    .selected_text()
                    .ok()
                    .flatten()
                    .as_deref()
                    .map(str::as_bytes)
                    == Some(&CLIPBOARD_PROOF_BYTES[..])
            {
                self.proof.selection = true;
                lines.push_str(&format!(
                    "TD-TERM-CLIPBOARD-SELECTION-READY bytes={}\n",
                    CLIPBOARD_PROOF_BYTES.len()
                ));
            }
        }
        if lines.is_empty() {
            return Ok(());
        }
        let mut out = std::io::stdout().lock();
        out.write_all(lines.as_bytes())
            .and_then(|()| out.flush())
            .map_err(|e| format!("write terminal clipboard marker: {e}"))
    }

    fn target_marker(&mut self) -> Result<Option<String>> {
        let Some(terminal) = self.model.as_ref() else {
            return Ok(None);
        };
        if self.viewport.offset(terminal.scrollback()) != 0 {
            return Ok(None);
        }
        let Some((row, column)) = clipboard_target(terminal) else {
            return Ok(None);
        };
        let (rows, columns) = self.cells.ok_or("terminal clipboard target has no grid")?;
        let row = u16::try_from(row).map_err(|_| "clipboard target row escaped u16")?;
        let column = u16::try_from(column).map_err(|_| "clipboard target column escaped u16")?;
        let target = (rows, columns, row, column);
        if self.proof.target == Some(target) {
            return Ok(None);
        }
        self.proof.target = Some(target);
        Ok(Some(format!(
            "{CLIPBOARD_TARGET_PREFIX} rows={rows} columns={columns} row={row} column={column} bytes={}\n",
            CLIPBOARD_PROOF_BYTES.len()
        )))
    }

    /// The proof's sync fired: the selection is the compositor's if the
    /// source it confirms is still the live one.
    fn synced(&mut self, id: u32) -> Result<()> {
        self.client.set_tag(id, Object::RetiredSync)?;
        let Some((sync, source, length)) = self.board.sync else {
            return Ok(());
        };
        if sync != id {
            return Ok(());
        }
        self.board.sync = None;
        if self.client.source() == Some(source) {
            let mut out = std::io::stdout().lock();
            out.write_all(format!("TD-TERM-CLIPBOARD-READY bytes={length}\n").as_bytes())
                .and_then(|()| out.flush())
                .map_err(|e| format!("write terminal clipboard marker: {e}"))?;
        }
        Ok(())
    }
}

impl App for Window {
    type Tag = Object;

    fn client(&mut self) -> &mut Client<Object> {
        &mut self.client
    }

    fn needs_descriptor(&self, _: &Message) -> Result<bool> {
        Ok(false)
    }

    fn descriptor_wait(&mut self) {
        self.drag = Drag::default();
    }

    fn tick(&mut self, now: u64) -> Result<()> {
        self.clock = now;
        Ok(())
    }

    fn event(&mut self, message: Message) -> Result<()> {
        match self.client.handle(&message, self.clock)? {
            Handled::Bound => {
                if self.client.seat().is_none() {
                    return Err("the compositor offers no wl_seat v5 or later".into());
                }
                self.client.set_title(TITLE)?;
                self.client.set_app_id(APP_ID)?;
                self.client.commit()?;
            }
            Handled::Configure { size, serial } => {
                let current = self.current.unwrap_or(self.fallback);
                // A zero axis is the compositor declining to choose it, and
                // a bare surface configure confirms what it last proposed.
                let next = match size {
                    Some((width, height)) => {
                        let width = usize::try_from(width).map_err(|_| "configure width")?;
                        let height = usize::try_from(height).map_err(|_| "configure height")?;
                        if width != 0 || height != 0 {
                            self.layout_configured = true;
                        }
                        Size {
                            width: if width == 0 { current.width } else { width },
                            height: if height == 0 { current.height } else { height },
                        }
                    }
                    None => current,
                };
                frame_bytes(next)?;
                self.current = Some(next);
                self.client.acknowledge(serial)?;
                self.needs_commit = true;
            }
            Handled::CloseRequested => {
                if self.options.profile != Profile::Desktop {
                    return Err("compositor requested that the terminal close".into());
                }
                // The child hears the hangup as the terminal's master closes.
                self.client.close();
            }
            Handled::FrameDone => {
                if let Some(frame) = self.frame.as_mut() {
                    frame.presented = true;
                    frame.shows_model = true;
                }
            }
            Handled::Keyboard(event) => match event {
                KeyboardEvent::Keymap(Ok(())) => self.keymap_ready = true,
                KeyboardEvent::Keymap(Err(error)) => {
                    self.keymap_ready = false;
                    // Before the child, a keyboard nobody can type with is
                    // a terminal that must not start.
                    if self.child.is_none() {
                        return Err(format!("the compositor's keymap was refused: {error}"));
                    }
                    let _ = writeln!(std::io::stderr().lock(), "td-term: keymap: {error}");
                }
                KeyboardEvent::Key {
                    serial,
                    key,
                    stroke,
                } => self.key(serial, key, &stroke)?,
                KeyboardEvent::Focus(false) => {
                    self.board.incoming = None;
                }
                KeyboardEvent::Refused(error) => {
                    let _ = writeln!(std::io::stderr().lock(), "td-term: key: {error}");
                }
                KeyboardEvent::Focus(true) | KeyboardEvent::Ready | KeyboardEvent::Held(_) => {}
            },
            Handled::Pointer(event) => self.pointer(event)?,
            Handled::Clipboard(event) => self.selection_event(data::Board::Clipboard, event),
            Handled::Primary(event) => self.selection_event(data::Board::Primary, event),
            Handled::Capabilities { keyboard, pointer } => {
                if !keyboard {
                    self.keymap_ready = false;
                    self.board.incoming = None;
                }
                if !pointer {
                    self.drag = Drag::default();
                    self.reported = Reported::default();
                }
            }
            Handled::SeatRemoved => {
                return Err("the compositor withdrew the seat while it was in use".into())
            }
            Handled::GlobalRemoved { required: true, .. } => {
                return Err("the compositor withdrew a global while it was in use".into())
            }
            Handled::Unhandled => match self.client.kind(message.object)? {
                td_ui::client::Kind::App(Object::Sync) if message.opcode == 0 => {
                    self.synced(message.object)?
                }
                _ => {
                    return Err(format!(
                        "unexpected Wayland event object={} opcode={}",
                        message.object, message.opcode
                    ))
                }
            },
            Handled::Done | Handled::GlobalRemoved { .. } => {}
        }
        Ok(())
    }

    fn end_turn(&mut self, now: u64, idle: bool) -> Result<()> {
        self.clock = now;
        // A close is the terminal's own ending: nothing drained after it
        // changes the status it closed with.
        if self.client.closed() {
            return Ok(());
        }
        self.drain()?;
        self.finished()?;
        if self.client.closed() {
            return Ok(());
        }
        if idle {
            if let Some(stroke) = self.client.repeat(now)? {
                // Rerouted per repetition: the mode and the view it asks
                // about may have changed since the press. A search takes
                // a repeat as it takes the press.
                let repeated = if self.search.is_some() {
                    self.check_search();
                    self.stale = true;
                    self.search_stroke(&stroke)
                } else {
                    self.route(&stroke.chord, stroke.text)?
                };
                if !repeated {
                    self.client.cancel_repeat();
                }
            }
        }
        self.transfers(now, idle)?;
        if self.child.is_none() {
            if self.may_start() {
                self.start()?;
            } else if now > HANDSHAKE_MS && self.options.profile != Profile::Desktop {
                return Err(format!(
                    "the terminal was not ready within {} s",
                    HANDSHAKE_MS / 1000
                ));
            }
        }
        self.markers()?;
        self.end_flash(now);
        let wait = self.next_wait(now);
        self.client
            .connection()
            .set_wait(Duration::from_millis(wait));
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        let Some(mut wanted) = self.wanted() else {
            return Ok(());
        };
        // Throttled on the frame in flight: only the latest state is drawn,
        // because what comes between is superseded before anything is
        // allocated for it.
        let resize = self.adopted != Some(wanted.size);
        if self.client.can_present() && (self.drawn != Some(wanted) || self.stale || resize) {
            // Drawn for the model the screen already shows, or not.
            let same_model = !self.stale && !resize;
            if resize {
                self.adopt(wanted.size)?;
                // The reflow moved the cells the link was on.
                wanted.link = self.hovered_link();
            }
            // A frame that takes the model's bell rings it, and so does
            // every frame until the flash is over. The flash starts when
            // the frame is submitted; one that is not puts the bell back.
            let took = self.model.as_mut().is_some_and(Terminal::take_bell);
            let clock = self.clock;
            let ringing = took || self.flash.is_some_and(|until| clock < until);
            let terminal = self
                .model
                .as_ref()
                .ok_or("the terminal has no model to draw")?;
            let viewport = self.viewport.offset(terminal.scrollback());
            let status = self.search_status();
            let snapshot = render::Snapshot::new(terminal, wanted.activated, ringing)
                .scrolled_back(viewport)
                .with_selection(self.shown_selection())
                .with_link(wanted.link)
                .with_status(
                    status.as_ref().map(|(text, _)| text.as_str()),
                    status
                        .as_ref()
                        .map_or(render::Edge::Bottom, |(_, edge)| *edge),
                );
            let (palette, font, outline) = (&self.palette, &self.font, &mut self.outline);
            let (width, height) = (wanted.size.width, wanted.size.height);
            let presented = self.client.present(width, height, &mut |pixels| {
                let outline = outline.as_mut().map(render::Zoom::face_mut);
                render::render_with(&snapshot, palette, font, outline, pixels, width, height)
            })?;
            if presented {
                if took {
                    self.flash = Some(clock.saturating_add(BELL_FLASH_MS));
                }
                self.drawn = Some(wanted);
                self.stale = false;
                self.needs_commit = false;
                let shows_model =
                    same_model && self.frame.as_ref().is_some_and(|frame| frame.shows_model);
                self.frame = self.client.presented().map(|buffer| Frame {
                    buffer,
                    presented: false,
                    shows_model,
                });
                return Ok(());
            }
            if took {
                if let Some(terminal) = self.model.as_mut() {
                    terminal.ring();
                }
            }
        }
        // An acknowledged configure is applied by the commit that follows
        // it; one no frame answered still gets a bare commit.
        if std::mem::take(&mut self.needs_commit) {
            self.client.commit()?;
        }
        Ok(())
    }
}

fn clipboard_target(terminal: &Terminal) -> Option<(usize, usize)> {
    let last_start = terminal
        .columns()
        .checked_sub(CLIPBOARD_PROOF_BYTES.len())?;
    for row in 0..terminal.rows() {
        for column in 0..=last_start {
            if CLIPBOARD_PROOF_BYTES
                .iter()
                .enumerate()
                .all(|(offset, expected)| {
                    terminal
                        .cell(row, column.saturating_add(offset))
                        .map(|cell| cell.scalar)
                        == Some(char::from(*expected))
                })
            {
                return Some((row, column));
            }
        }
    }
    None
}

fn clipboard_sent_marker(enabled: bool, payload: &str) -> Option<String> {
    (enabled && payload.as_bytes() == CLIPBOARD_PROOF_BYTES).then(|| {
        format!(
            "TD-TERM-CLIPBOARD-SENT bytes={}\n",
            CLIPBOARD_PROOF_BYTES.len()
        )
    })
}

/// A paste as the child receives it: text only — a control character in a
/// paste is a command nobody typed — and bracketed when the child asked.
fn paste_input(bytes: Vec<u8>, bracketed: bool) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(&bytes).map_err(|_| "paste is not UTF-8")?;
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
    {
        return Err("paste contains terminal control characters".into());
    }
    if bytes.is_empty() {
        return Ok(bytes);
    }
    let size = bytes.len().saturating_add(if bracketed { 12 } else { 0 });
    if size > keys::MAX_INPUT_BYTES {
        return Err("encoded paste exceeds the input queue bound".into());
    }
    if !bracketed {
        return Ok(bytes);
    }
    let mut result = Vec::with_capacity(size);
    result.extend_from_slice(b"\x1b[200~");
    result.extend_from_slice(&bytes);
    result.extend_from_slice(b"\x1b[201~");
    Ok(result)
}

/// A shell's convention for a status: the code, or 128 and the signal.
fn exit_code(status: ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => u8::try_from(code & 0xff).unwrap_or(1),
        (None, Some(signal)) => u8::try_from(128 + (signal & 0x7f)).unwrap_or(1),
        (None, None) => 1,
    }
}

/// A child ending ends td's terminal; the words say which ending it was.
fn ended(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("the terminal's child exited with status {code}"),
        None => format!("the terminal's child was killed by a signal ({status})"),
    }
}

/// How every row of a report begins; the boot oracle reads a console line
/// that begins so as a record, whatever follows.
const LAST_SCREEN_PREFIX: &str = "td-term: last screen";

/// The most of a program's name a report carries.
const MAX_REPORTED_PROGRAM_CHARS: usize = 32;

/// The name a `--command` child is reported under, and `None` for the
/// default shell: its final path component, bounded, with any character a
/// report would not print made a space.
fn launched_program_name(command: &[OsString]) -> Option<String> {
    let program = command.first()?;
    let name = Path::new(program)
        .file_name()
        .unwrap_or(program.as_os_str())
        .to_string_lossy();
    Some(
        name.chars()
            .take(MAX_REPORTED_PROGRAM_CHARS)
            .map(|character| {
                if td_ui::reportable::reportable(character) {
                    character
                } else {
                    ' '
                }
            })
            .collect(),
    )
}

/// The log lines for a `--command` child that ended badly, and nothing for
/// any other exit: the window closes with the session, and a program run
/// by `--command` has no other place to be heard. The active screen's rows
/// at exit, trailing blanks and the blank rows below the last written one
/// dropped, each behind `td-term: last screen (<program>): `, in ONE buffer
/// beginning on a fresh line. A character a report would not print is a
/// space, since a line separator or a direction control in a log line is a
/// forged record.
fn last_screen_report(
    program: Option<&str>,
    status: ExitStatus,
    model: Option<&Terminal>,
) -> Option<String> {
    if status.success() {
        return None;
    }
    let program = program?;
    let snapshot = render::Snapshot::new(model?, false, false);
    let (rows, columns) = (snapshot.rows(), snapshot.columns());
    let prefix = format!("{LAST_SCREEN_PREFIX} ({program}): ");
    let mut report = String::with_capacity(1 + rows * (prefix.len() + columns + 1));
    report.push('\n');
    let mut line = String::with_capacity(columns);
    let mut kept = 0;
    for row in 0..rows {
        line.clear();
        for column in 0..columns {
            let scalar = snapshot.cell(row, column).scalar;
            line.push(if td_ui::reportable::reportable(scalar) {
                scalar
            } else {
                ' '
            });
        }
        let written = line.trim_end();
        report.push_str(&prefix);
        report.push_str(written);
        report.push('\n');
        if !written.is_empty() {
            kept = report.len();
        }
    }
    report.truncate(kept);
    (!report.is_empty()).then_some(report)
}

fn cmdline_has_clipboard_proof(bytes: &[u8]) -> bool {
    bytes
        .split(u8::is_ascii_whitespace)
        .any(|word| word == CLIPBOARD_PROOF_CMDLINE_TOKEN)
}

/// Whether the boot's command line asked for the clipboard proof. A host
/// without the file, or one that cannot be read, has not asked.
fn clipboard_proof_enabled(path: &Path) -> Result<bool> {
    let Ok(file) = File::open(path) else {
        return Ok(false);
    };
    let limit = u64::try_from(MAX_CMDLINE_BYTES.saturating_add(1))
        .map_err(|_| "terminal command-line limit escaped u64")?;
    let mut bytes = Vec::with_capacity(MAX_CMDLINE_BYTES.saturating_add(1));
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read terminal proof authority {}: {error}", path.display()))?;
    if bytes.len() > MAX_CMDLINE_BYTES {
        return Err(format!(
            "terminal proof authority {} exceeded {MAX_CMDLINE_BYTES} bytes",
            path.display()
        ));
    }
    Ok(cmdline_has_clipboard_proof(&bytes))
}

/// The display the child and a followed link's browser are told: the path
/// the terminal dialled, made absolute, since their libwayland resolves a
/// relative WAYLAND_DISPLAY under XDG_RUNTIME_DIR rather than td-term's
/// directory. An inherited WAYLAND_SOCKET is refused, since that descriptor
/// crossed exec without close-on-exec and the child would hold the
/// terminal's own connection.
fn child_display(endpoint: &Endpoint) -> Result<String> {
    let Endpoint::Path(path) = endpoint else {
        return Err(
            "td-term does not take an inherited WAYLAND_SOCKET, which its child would \
             inherit; name the display with WAYLAND_DISPLAY or --socket"
                .into(),
        );
    };
    std::path::absolute(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| "Wayland socket path is not UTF-8".into())
}

/// Runs the terminal; answers the desktop profile's exit status. td's
/// profile ends only in error.
pub fn run(options: Options) -> Result<u8> {
    let proof = clipboard_proof_enabled(Path::new(PROC_CMDLINE))?;
    let endpoint = match &options.socket {
        // A desktop's relative socket is a display name, as WAYLAND_DISPLAY's.
        Some(path) if options.profile == Profile::Desktop && path.is_relative() => {
            td_ui::wayland::endpoint(
                None,
                Some(path.clone().into_os_string()),
                std::env::var_os("XDG_RUNTIME_DIR"),
            )
            .map_err(|e| format!("--socket {}: {e}", path.display()))?
        }
        Some(path) => Endpoint::Path(path.clone()),
        None => td_ui::wayland::endpoint(
            std::env::var_os("WAYLAND_SOCKET"),
            std::env::var_os("WAYLAND_DISPLAY"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )?,
    };
    let display = child_display(&endpoint)?;
    let stream = td_ui::wayland::connect(endpoint)?;
    let mut window = Window::new(stream, options, display)?;
    window.proof.enabled = proof;
    // The pinned outline face in four styles, at `--font-size` on its own
    // cell or fitted to the bitmap cell so the grid and every rule stay
    // Unifont's; without it, Unifont.
    let setting = std::env::var_os(td_ui::pinned_face::SETTING);
    let font_size = window.options.font_size;
    let start = start_sizing(font_size, &window.font);
    let outline = td_ui::pinned_face::styles_or_note("td-term", start, setting.as_deref());
    if outline.is_none() && font_size.is_some() {
        let _ = writeln!(
            std::io::stderr().lock(),
            "td-term: --font-size sizes the outline face; drawing in Unifont's cell"
        );
    }
    window.set_outline(outline, start)?;
    td_ui::client::run(&mut window)?;
    Ok(window.exit)
}

pub fn selftest() -> Result<()> {
    let font = td_ui::font::pinned()?;
    let cell = render::cell_size(&font, None);
    let fallback = default_size(cell)?;
    let (rows, columns) = grid(fallback, cell)?;
    let expected = (
        u16::try_from(DEFAULT_ROWS).map_err(|_| "default rows escape a grid")?,
        u16::try_from(DEFAULT_COLUMNS).map_err(|_| "default columns escape a grid")?,
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
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::FileExt;
    use std::os::unix::net::UnixStream;
    use td_ui::client::{Kind, SHM, SURFACE, SYNC, TOPLEVEL, XDG_SURFACE};
    use td_ui::wayland::{backing_file, peer};
    use td_ui::wire::{self, Builder, Cursor};

    const SEAT: u32 = 10;
    const DEVICE: u32 = 12;
    const PRIMARY_MANAGER: u32 = 13;
    const PRIMARY_DEVICE: u32 = 14;
    const POINTER: u32 = 15;
    const KEYBOARD: u32 = 16;
    const SHIFT: u32 = 1;
    const CONTROL: u32 = 4;
    const ALT: u32 = 8;

    fn message(object: u32, opcode: u16, words: &[u32]) -> Message {
        let mut body = Builder::new();
        for word in words {
            body.u32(*word);
        }
        wire::take(&mut body.message(object, opcode).unwrap())
            .unwrap()
            .unwrap()
    }

    fn global(id: u32, name: &str, version: u32) -> Message {
        let mut body = Builder::new();
        body.u32(id);
        body.string(name).unwrap();
        body.u32(version);
        wire::take(&mut body.message(td_ui::client::REGISTRY, 0).unwrap())
            .unwrap()
            .unwrap()
    }

    fn text(object: u32, opcode: u16, value: &str) -> Message {
        let mut body = Builder::new();
        body.string(value).unwrap();
        wire::take(&mut body.message(object, opcode).unwrap())
            .unwrap()
            .unwrap()
    }

    /// td's own keymap, the one its compositor publishes.
    fn td_keymap() -> &'static str {
        include_str!("../../td-compositor/src/keyboard.rs")
            .split_once("pub const XKB_KEYMAP: &str = r#\"")
            .unwrap()
            .1
            .split_once("\"#;")
            .unwrap()
            .0
    }

    /// A window bound to a scripted compositor offering a seat, a
    /// clipboard and a primary selection, both devices created:
    /// `(window, peer)`.
    fn fixture() -> (Window, UnixStream) {
        fixture_for(Profile::Td {
            ready_socket: std::env::temp_dir().join("td-term-unused-ready"),
        })
    }

    fn fixture_for(profile: Profile) -> (Window, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        theirs
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let options = Options {
            socket: None,
            profile,
            working_directory: None,
            command: Vec::new(),
            font_size: None,
        };
        let mut window = Window::new(ours, options, "/run/wayland-test".into()).unwrap();
        for event in [
            global(1, "wl_compositor", 4),
            global(2, "wl_shm", 1),
            global(3, "xdg_wm_base", 1),
            global(4, "wl_seat", 7),
            global(5, "wl_data_device_manager", 3),
            global(6, "zwp_primary_selection_device_manager_v1", 1),
        ] {
            window.event(event).unwrap();
        }
        window.event(message(SYNC, 0, &[0])).unwrap();
        window.event(message(DISPLAY, 1, &[SYNC])).unwrap();
        window.event(message(SHM, 0, &[1])).unwrap();
        window.event(message(SEAT, 0, &[3])).unwrap();
        assert_eq!(window.client.keyboard(), Some(KEYBOARD));
        assert_eq!(window.client.pointer(), Some(POINTER));
        peer::drain(&theirs).unwrap();
        (window, theirs)
    }

    fn configure(window: &mut Window, width: u32, height: u32, activated: bool) {
        let mut words = vec![width, height];
        if activated {
            words.extend_from_slice(&[4, 4]);
        } else {
            words.push(0);
        }
        window.event(message(TOPLEVEL, 0, &words)).unwrap();
        window.event(message(XDG_SURFACE, 0, &[77])).unwrap();
    }

    /// The compositor finishes with the last frame: callback, then release.
    fn complete(window: &mut Window) {
        let callback = window.client.frame_callback().unwrap();
        window.event(message(callback, 0, &[0])).unwrap();
        window.event(message(DISPLAY, 1, &[callback])).unwrap();
        let buffer = window.client.presented().unwrap();
        window.event(message(buffer, 0, &[])).unwrap();
    }

    fn keymap(window: &mut Window) {
        let source = td_keymap();
        let file = backing_file(&std::env::temp_dir(), source.len() + 1).unwrap();
        file.write_all_at(source.as_bytes(), 0).unwrap();
        peer::push_descriptor(window.client.connection(), OwnedFd::from(file)).unwrap();
        let size = u32::try_from(source.len() + 1).unwrap();
        window.event(message(KEYBOARD, 0, &[1, size])).unwrap();
    }

    /// Focus, then a modifier snapshot: presses translate from here.
    fn focus(window: &mut Window, serial: u32) {
        window
            .event(message(KEYBOARD, 1, &[serial, SURFACE, 0]))
            .unwrap();
        modifiers(window, 0);
    }

    fn modifiers(window: &mut Window, depressed: u32) {
        window
            .event(message(KEYBOARD, 4, &[0, depressed, 0, 0, 0]))
            .unwrap();
    }

    fn press(window: &mut Window, key: u32) {
        window.event(message(KEYBOARD, 3, &[9, 0, key, 1])).unwrap();
        window.event(message(KEYBOARD, 3, &[9, 0, key, 0])).unwrap();
    }

    /// A window presented at a chosen 640 by 320 tile with td's keymap.
    fn presented() -> (Window, UnixStream) {
        let (mut window, peer) = fixture();
        configure(&mut window, 640, 320, true);
        window.draw().unwrap();
        complete(&mut window);
        keymap(&mut window);
        (window, peer)
    }

    fn history(window: &mut Window) {
        let lines: Vec<u8> = (0..60)
            .flat_map(|n| format!("{n}\r\n").into_bytes())
            .collect();
        window.output(&lines).unwrap();
    }

    #[test]
    fn a_flooding_child_cannot_hold_the_turn() {
        let (mut window, _peer) = presented();
        window.drain().unwrap();
        assert!(!window.backlog, "an empty queue leaves no backlog");
        // A child that refills the queue as fast as a turn empties it: one
        // event past the bound is still queued when the turn gives way.
        let sender = window.sender.clone().unwrap();
        for _ in 0..=MAX_DRAINED_PER_TURN {
            sender.send(Event::Output(b"y\r\n".to_vec())).unwrap();
        }
        window.drain().unwrap();
        assert!(window.backlog, "the drain stopped at its bound");
        assert_eq!(window.next_wait(0), 1, "and the next turn comes at once");
        assert!(window.events.try_recv().is_ok(), "the rest waits a turn");
        window.drain().unwrap();
        assert!(!window.backlog);
        assert!(window.next_wait(0) > 1);
    }

    #[test]
    fn the_child_is_told_the_dialled_path_and_never_an_inherited_socket() {
        assert_eq!(
            child_display(&Endpoint::Path("/run/user/1000/wayland-1".into())).unwrap(),
            "/run/user/1000/wayland-1"
        );
        assert_eq!(
            PathBuf::from(child_display(&Endpoint::Path("wayland-9".into())).unwrap()),
            std::env::current_dir().unwrap().join("wayland-9"),
            "a relative socket is told as td-term resolved it"
        );
        assert!(child_display(&Endpoint::Inherited(3))
            .unwrap_err()
            .contains("WAYLAND_SOCKET"));
    }

    #[test]
    fn a_size_adopted_but_never_presented_is_adopted_again() {
        let (mut window, _peer) = presented();
        let cells = window.cells;
        // A configure the PTY followed whose frame never presented.
        window
            .adopt(Size {
                width: 320,
                height: 160,
            })
            .unwrap();
        assert_eq!(window.cells, Some((10, 40)));
        // Back at the size already drawn: the picture is current, the grid
        // is not, and the grid is what the child reads -- so it is not ready.
        assert!(!window.ready());
        window.draw().unwrap();
        assert_eq!(window.cells, cells);
    }

    #[test]
    fn readiness_needs_a_chosen_size_a_whole_frame_and_a_keymap() {
        let (mut window, _peer) = fixture();
        // The first configure declines to choose: the frame at the fallback
        // maps the surface but is not readiness.
        configure(&mut window, 0, 0, false);
        window.draw().unwrap();
        assert_eq!(window.cells, Some((24, 80)));
        complete(&mut window);
        assert!(!window.ready());
        configure(&mut window, 640, 320, true);
        window.draw().unwrap();
        assert_eq!(window.cells, Some((20, 80)));
        // Half an answer is not an answer: the callback alone.
        let callback = window.client.frame_callback().unwrap();
        window.event(message(callback, 0, &[0])).unwrap();
        assert!(!window.ready());
        let buffer = window.client.presented().unwrap();
        window.event(message(buffer, 0, &[])).unwrap();
        assert!(window.ready());
        assert!(!window.presented(), "no keymap yet");
        keymap(&mut window);
        assert!(window.presented());
        // A seat that withdraws its keyboard is no longer a terminal.
        window.event(message(SEAT, 0, &[1])).unwrap();
        assert!(!window.presented());
    }

    /// A desktop's floating window is never given a size, and a compositor
    /// may keep its first buffer or never call back for it: the child
    /// starts once that frame is committed. td's waits for its readiness.
    #[test]
    fn a_desktop_terminal_starts_at_its_first_committed_frame() {
        let (mut window, _peer) = fixture_for(Profile::Desktop);
        assert!(!window.may_start());
        configure(&mut window, 0, 0, false);
        window.draw().unwrap();
        assert_eq!(window.cells, Some((24, 80)));
        assert!(window.may_start(), "no callback, release or keymap asked");
        let (mut window, _peer) = fixture();
        configure(&mut window, 0, 0, false);
        window.draw().unwrap();
        complete(&mut window);
        keymap(&mut window);
        assert!(!window.may_start(), "td's waits for a chosen size");
    }

    /// The handshake bound is td's supervisor's; a desktop's window may wait
    /// unshown on another workspace as long as it likes.
    #[test]
    fn only_tds_terminal_has_a_handshake_bound() {
        let (mut window, _peer) = fixture_for(Profile::Desktop);
        window.end_turn(HANDSHAKE_MS + 1, true).unwrap();
        let (mut window, _peer) = fixture();
        assert!(window
            .end_turn(HANDSHAKE_MS + 1, true)
            .unwrap_err()
            .contains("not ready"));
    }

    fn ended_child(window: &mut Window, raw: i32) {
        use std::os::unix::process::ExitStatusExt;
        window.child = Some(Child {
            drained: false,
            status: Some(ExitStatus::from_raw(raw)),
            program: None,
            _threads: Vec::new(),
            _ready: None,
        });
    }

    /// The desktop closes with its child's status, a shell's convention for
    /// a signal, without waiting for a background job to let go of the
    /// terminal (the output undrained); td's profile ends in the error its
    /// supervisor reads, once drained.
    #[test]
    fn a_desktop_terminal_ends_with_its_childs_status() {
        for (raw, code) in [(0, 0), (3 << 8, 3), (9, 137)] {
            let (mut window, _peer) = fixture_for(Profile::Desktop);
            ended_child(&mut window, raw);
            window.end_turn(0, true).unwrap();
            assert!(window.client.closed());
            assert_eq!(window.exit, code);
        }
        let (mut window, _peer) = fixture();
        ended_child(&mut window, 3 << 8);
        window.end_turn(0, true).unwrap();
        assert!(!window.client.closed(), "td's waits for the output");
        if let Some(child) = window.child.as_mut() {
            child.drained = true;
        }
        assert_eq!(
            window.end_turn(0, true).unwrap_err(),
            "the terminal's child exited with status 3"
        );
        assert!(!window.client.closed());
    }

    /// Closing the window ends a desktop terminal, its child hung up as the
    /// master closes; td's terminal treats it as an error.
    #[test]
    fn a_close_request_closes_a_desktop_terminal() {
        let (mut window, _peer) = fixture_for(Profile::Desktop);
        window.event(message(TOPLEVEL, 1, &[])).unwrap();
        assert!(window.client.closed());
        // The child's own ending, arriving in the same turn, is not the
        // status the close said.
        ended_child(&mut window, 3 << 8);
        window.end_turn(0, true).unwrap();
        assert_eq!(window.exit, 0);
        let (mut window, _peer) = fixture();
        assert!(window.event(message(TOPLEVEL, 1, &[])).is_err());
    }

    #[test]
    fn one_chosen_axis_chooses_and_a_bare_configure_keeps_the_size() {
        let (mut window, _peer) = fixture();
        configure(&mut window, 0, 160, false);
        assert!(window.layout_configured);
        assert_eq!(
            window.current,
            Some(Size {
                width: 640,
                height: 160
            })
        );
        window.event(message(XDG_SURFACE, 0, &[78])).unwrap();
        assert_eq!(
            window.current,
            Some(Size {
                width: 640,
                height: 160
            })
        );
        assert!(window.needs_commit);
    }

    #[test]
    fn a_configure_too_large_to_serve_is_refused_before_allocating() {
        let (mut window, _peer) = fixture();
        window
            .event(message(TOPLEVEL, 0, &[20_000, 20_000, 0]))
            .unwrap();
        assert!(window.event(message(XDG_SURFACE, 0, &[1])).is_err());
        assert!(window.model.is_none());
    }

    #[test]
    fn losing_focus_redraws_at_the_same_size() {
        let (mut window, peer) = presented();
        assert!(window.ready());
        configure(&mut window, 640, 320, false);
        assert!(!window.ready(), "a frame drawn focused is not this surface");
        peer::drain(&peer).unwrap();
        window.draw().unwrap();
        assert_eq!(window.drawn.map(|drawn| drawn.activated), Some(false));
        assert_eq!(window.cells, Some((20, 80)));
    }

    #[test]
    fn an_acknowledged_configure_nothing_redraws_is_committed() {
        let (mut window, peer) = presented();
        configure(&mut window, 640, 320, true);
        peer::drain(&peer).unwrap();
        window.draw().unwrap();
        let (requests, _) = peer::drain(&peer).unwrap();
        assert!(requests
            .iter()
            .any(|m| m.object == SURFACE && m.opcode == 6));
        assert!(!window.needs_commit);
    }

    #[test]
    fn keys_reach_the_child_through_tds_keymap_and_the_encoder() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        press(&mut window, 30);
        modifiers(&mut window, SHIFT);
        press(&mut window, 30);
        modifiers(&mut window, CONTROL);
        press(&mut window, 46);
        modifiers(&mut window, 0);
        press(&mut window, 103);
        press(&mut window, 28);
        assert_eq!(window.input.take_for_test(), b"aA\x03\x1b[A\r");
        // The child's DECCKM changes the spelling of the next press.
        window.output(b"\x1b[?1h").unwrap();
        press(&mut window, 103);
        assert_eq!(window.input.take_for_test(), b"\x1bOA");
    }

    /// foot's font chords are td-term's, and with no outline face, as
    /// here, do nothing: the selection stays, and the encoder, which sends
    /// the child nothing for any of them, never sees them.
    #[test]
    fn font_chords_without_an_outline_face_do_nothing() {
        assert_eq!(zoom_to("C-="), Some(render::ZoomTo::In));
        assert_eq!(zoom_to("C-+"), Some(render::ZoomTo::In));
        assert_eq!(zoom_to("C--"), Some(render::ZoomTo::Out));
        assert_eq!(zoom_to("C-0"), Some(render::ZoomTo::Start));
        for other in ["C-M-=", "=", "C-S-0", "C-1"] {
            assert_eq!(zoom_to(other), None, "{other}");
        }
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.output(b"Welcome").unwrap();
        let selection = Some(render::Selection {
            anchor: (0, 0),
            extent: (0, 6),
        });
        window.selection = selection;
        // Equals, minus and zero with Control, and equals with Shift too.
        modifiers(&mut window, CONTROL);
        for key in [13, 12, 11] {
            press(&mut window, key);
        }
        modifiers(&mut window, CONTROL | SHIFT);
        press(&mut window, 13);
        modifiers(&mut window, 0);
        assert!(window.input.take_for_test().is_empty());
        assert!(!bell(&mut window));
        assert_eq!(window.cells, Some((20, 80)));
        assert_eq!(window.selection, selection);
        for chord in ["C-=", "C-+", "C--", "C-0"] {
            assert_eq!(
                keys::action(chord, None, keys::Modes::default(), false),
                keys::Action::Silent,
                "{chord}"
            );
        }
    }

    /// The grid is the surface divided by the window's cell, the outline
    /// face's when there is one, and a new cell is adopted at the next
    /// draw as a resize is: the PTY is set to its grid before the frame.
    #[test]
    fn the_grid_is_laid_on_the_windows_cell() {
        let (mut window, _peer) = presented();
        assert_eq!(window.cells, Some((20, 80)));
        window.cell = (16, 32);
        window.adopted = None;
        window.draw().unwrap();
        assert_eq!(window.cells, Some((10, 40)));
        let winsize = window.pty.window().unwrap();
        assert_eq!((winsize.rows, winsize.columns), (10, 40));
        let model = window.model.as_ref().unwrap();
        assert_eq!((model.rows(), model.columns()), (10, 40));
        // A pointer is hit-tested by the same cell.
        let fixed = |pixels: u32| i32::try_from(pixels * 256).unwrap();
        assert_eq!(window.cell_at((fixed(33), fixed(40))), Some((1, 2)));
        assert_eq!(
            default_size((16, 32)).unwrap(),
            Size {
                width: 1280,
                height: 768
            }
        );
    }

    /// Re-adopting a grid the PTY and model already have, as a zoom by half
    /// a point often asks, touches neither: a reflow would reset the
    /// scrolling margins a child set.
    #[test]
    fn re_adopting_the_same_grid_keeps_the_scrolling_margins() {
        let (mut window, _peer) = presented();
        // Margins on rows 2 to 5; a line feed at the bottom one scrolls
        // only them, so the 'a' on row 2 scrolls out.
        window.output(b"top\x1b[2;5r\x1b[2;1Ha\x1b[5;1H").unwrap();
        window.cell = (8, 16);
        window.adopted = None;
        window.draw().unwrap();
        assert_eq!(window.adopted, window.current);
        window.output(b"\n").unwrap();
        let model = window.model.as_ref().unwrap();
        assert_eq!(model.cell(1, 0).map(|cell| cell.scalar), Some(' '));
        assert_eq!(model.cell(0, 0).map(|cell| cell.scalar), Some('t'));
    }

    /// The fallback grid is 80x24 cells, or what of it a frame within the
    /// raster's ceilings holds.
    #[test]
    fn the_fallback_grid_fits_the_raster_ceilings() {
        assert_eq!(
            default_size((8, 16)).unwrap(),
            Size {
                width: 640,
                height: 384
            }
        );
        // 192 points of the pinned face: a 154x338 cell.
        for cell in [(48, 106), (154, 338), (512, 512)] {
            let size = default_size(cell).unwrap();
            assert!(frame_bytes(size).is_ok(), "{cell:?}: {size:?}");
            assert_eq!(size.width % cell.0, 0);
            assert_eq!(size.height % cell.1, 0);
        }
        assert_eq!(
            default_size((48, 106)).unwrap(),
            Size {
                width: 3840,
                height: 2120
            }
        );
        assert!(default_size((0, 16)).is_err());
    }

    #[test]
    fn shift_page_up_moves_the_view_and_typing_returns_it() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        history(&mut window);
        modifiers(&mut window, SHIFT);
        press(&mut window, 104);
        assert!(window.viewport.viewing(window.history()));
        assert!(
            window.input.take_for_test().is_empty(),
            "a scroll sends nothing"
        );
        modifiers(&mut window, 0);
        press(&mut window, 30);
        assert!(!window.viewport.viewing(window.history()));
        assert_eq!(window.input.take_for_test(), b"a");
    }

    /// The pointer at cell (row, column), a frame closing the move.
    fn hover(window: &mut Window, row: u32, column: u32) {
        window
            .event(message(
                POINTER,
                2,
                &[0, column * 8 * 256 + 128, row * 16 * 256 + 128],
            ))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
    }

    /// One button's press or release, and its frame.
    fn button(window: &mut Window, button: u32, pressed: bool) {
        window
            .event(message(POINTER, 3, &[7, 0, button, u32::from(pressed)]))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
    }

    /// While the child asks for reports, its presses and releases are
    /// reported at the pointer's cell rather than selecting, pasting or
    /// following a link, and the wheel is its wheel; once it stops asking
    /// they are td-term's again.
    #[test]
    fn the_child_hears_the_pointer_while_it_asks_for_reports() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window
            .output(b"see https://e.example/x\x1b[?1000h")
            .unwrap();
        hover(&mut window, 0, 0);
        button(&mut window, LEFT_BUTTON, true);
        hover(&mut window, 0, 6);
        button(&mut window, LEFT_BUTTON, false);
        assert_eq!(window.input.take_for_test(), b"\x1b[M !!\x1b[M#'!");
        assert_eq!(window.selection, None);
        // Middle and right, with Control over the link: reported, so
        // nothing is pasted and no link opens.
        modifiers(&mut window, CONTROL);
        hover(&mut window, 0, 8);
        button(&mut window, MIDDLE_BUTTON, true);
        button(&mut window, MIDDLE_BUTTON, false);
        button(&mut window, RIGHT_BUTTON, true);
        button(&mut window, RIGHT_BUTTON, false);
        modifiers(&mut window, 0);
        assert_eq!(
            window.input.take_for_test(),
            b"\x1b[M1)!\x1b[M3)!\x1b[M2)!\x1b[M3)!"
        );
        assert!(!bell(&mut window), "no link was followed");
        assert!(window.board.incoming.is_none(), "nothing was pasted");
        // A notch of the wheel each way, the view unmoved.
        history(&mut window);
        window
            .event(message(POINTER, 8, &[0, (-1i32) as u32]))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        window.event(message(POINTER, 8, &[0, 2])).unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        assert_eq!(window.input.take_for_test(), b"\x1b[M`)!\x1b[Ma)!\x1b[Ma)!");
        assert_eq!(window.viewport.offset(window.history()), 0);
        // Reset: the press selects again and the wheel scrolls.
        window.output(b"\x1b[?1000l").unwrap();
        hover(&mut window, 0, 0);
        button(&mut window, LEFT_BUTTON, true);
        hover(&mut window, 0, 3);
        button(&mut window, LEFT_BUTTON, false);
        assert!(window.input.take_for_test().is_empty());
        assert!(window.selection.is_some());
        window
            .event(message(POINTER, 8, &[0, (-1i32) as u32]))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        assert_eq!(window.viewport.offset(window.history()), 3);
    }

    /// Shift keeps a gesture td-term's, as foot's selection override does,
    /// and so does a view scrolled back off the child's screen; a reported
    /// press's release is the child's whatever is held by then.
    #[test]
    fn shift_and_a_scrolled_view_keep_the_pointer_td_terms() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.output(b"Welcome\x1b[?1002h").unwrap();
        modifiers(&mut window, SHIFT);
        hover(&mut window, 0, 0);
        button(&mut window, LEFT_BUTTON, true);
        hover(&mut window, 0, 6);
        button(&mut window, LEFT_BUTTON, false);
        assert!(window.input.take_for_test().is_empty());
        assert_eq!(window.selected_text().unwrap().as_deref(), Some("Welcome"));
        modifiers(&mut window, 0);
        // Pressed plainly, released with Shift: both the child's.
        button(&mut window, LEFT_BUTTON, true);
        modifiers(&mut window, SHIFT);
        button(&mut window, LEFT_BUTTON, false);
        modifiers(&mut window, 0);
        assert_eq!(window.input.take_for_test(), b"\x1b[M '!\x1b[M''!");
        // Scrolled back, a press selects rather than reports.
        history(&mut window);
        modifiers(&mut window, SHIFT);
        press(&mut window, 104);
        modifiers(&mut window, 0);
        assert!(window.viewport.viewing(window.history()));
        button(&mut window, LEFT_BUTTON, true);
        button(&mut window, LEFT_BUTTON, false);
        assert!(window.input.take_for_test().is_empty());
    }

    /// Mode 1002 reports motion with a reported button held, once a cell;
    /// 1003 reports it with none held too, but not under Shift; neither
    /// reports motion within a cell.
    #[test]
    fn motion_is_reported_once_a_cell_as_the_mode_asks() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.output(b"\x1b[?1002;1006h").unwrap();
        hover(&mut window, 1, 1);
        assert!(window.input.take_for_test().is_empty(), "no button held");
        button(&mut window, LEFT_BUTTON, true);
        window
            .event(message(POINTER, 2, &[0, 8 * 256 + 200, 16 * 256 + 200]))
            .unwrap();
        hover(&mut window, 1, 2);
        hover(&mut window, 2, 2);
        button(&mut window, LEFT_BUTTON, false);
        assert_eq!(
            window.input.take_for_test(),
            b"\x1b[<0;2;2M\x1b[<32;3;2M\x1b[<32;3;3M\x1b[<0;3;3m"
        );
        // Middle and right drag as themselves, Shift included: the gesture
        // is the child's.
        button(&mut window, MIDDLE_BUTTON, true);
        modifiers(&mut window, SHIFT);
        hover(&mut window, 2, 3);
        modifiers(&mut window, 0);
        button(&mut window, MIDDLE_BUTTON, false);
        button(&mut window, RIGHT_BUTTON, true);
        hover(&mut window, 2, 4);
        button(&mut window, RIGHT_BUTTON, false);
        assert_eq!(
            window.input.take_for_test(),
            b"\x1b[<1;3;3M\x1b[<37;4;3M\x1b[<1;4;3m\x1b[<2;4;3M\x1b[<34;5;3M\x1b[<2;5;3m"
        );
        window.output(b"\x1b[?1003h").unwrap();
        hover(&mut window, 4, 5);
        hover(&mut window, 4, 5);
        modifiers(&mut window, SHIFT);
        hover(&mut window, 5, 5);
        modifiers(&mut window, 0);
        assert_eq!(window.input.take_for_test(), b"\x1b[<35;6;5M");
    }

    /// A reported press is the child's gesture to its end: its release is
    /// the child's though the view has scrolled back meanwhile, and is
    /// absorbed, rather than ending a selection, once the child has
    /// stopped asking.
    #[test]
    fn a_reported_press_is_released_to_the_child_wherever_the_view_is() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        history(&mut window);
        window.output(b"\x1b[?1000h").unwrap();
        hover(&mut window, 0, 0);
        button(&mut window, LEFT_BUTTON, true);
        modifiers(&mut window, SHIFT);
        press(&mut window, 104);
        modifiers(&mut window, 0);
        assert!(window.viewport.viewing(window.history()));
        button(&mut window, LEFT_BUTTON, false);
        assert_eq!(window.input.take_for_test(), b"\x1b[M !!\x1b[M#!!");
        // Back at the live screen: the child stops asking mid-press, and
        // the release is absorbed, leaving a selection made since then
        // untouched.
        press(&mut window, 107);
        assert!(!window.viewport.viewing(window.history()));
        button(&mut window, LEFT_BUTTON, true);
        assert_eq!(window.input.take_for_test(), b"\x1b[M !!");
        window.output(b"\x1b[?1000l").unwrap();
        let selected = selection((0, 0), (0, 1));
        window.selection = selected;
        hover(&mut window, 0, 4);
        button(&mut window, LEFT_BUTTON, false);
        assert!(window.input.take_for_test().is_empty());
        assert_eq!(window.selection, selected);
        assert!(!window.drag.left_down);
    }

    /// Leaving releases what the child was told is down, at the last cell
    /// it was told of, so re-entering is not a drag and a selection after
    /// it is td-term's.
    #[test]
    fn leaving_releases_the_reported_buttons() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.output(b"Welcome\x1b[?1002h").unwrap();
        hover(&mut window, 0, 2);
        button(&mut window, RIGHT_BUTTON, true);
        window.event(message(POINTER, 1, &[8, SURFACE])).unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        assert_eq!(window.input.take_for_test(), b"\x1b[M\"#!\x1b[M##!");
        window
            .event(message(POINTER, 0, &[9, SURFACE, 128, 128]))
            .unwrap();
        hover(&mut window, 0, 5);
        assert!(window.input.take_for_test().is_empty(), "not a drag");
        modifiers(&mut window, SHIFT);
        hover(&mut window, 0, 0);
        button(&mut window, LEFT_BUTTON, true);
        hover(&mut window, 0, 6);
        button(&mut window, LEFT_BUTTON, false);
        modifiers(&mut window, 0);
        assert!(window.input.take_for_test().is_empty());
        assert_eq!(window.selected_text().unwrap().as_deref(), Some("Welcome"));
    }

    /// Leaving releases at the cell the child last heard of, not where
    /// unreported motion took the pointer since.
    #[test]
    fn leaving_releases_at_the_last_cell_the_child_was_told_of() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.output(b"\x1b[?1000h").unwrap();
        hover(&mut window, 0, 2);
        button(&mut window, LEFT_BUTTON, true);
        hover(&mut window, 0, 5);
        window.event(message(POINTER, 1, &[8, SURFACE])).unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        assert_eq!(window.input.take_for_test(), b"\x1b[M #!\x1b[M##!");
    }

    /// A motion that sends nothing names no cell: once the child asks for
    /// drags, motion at the cell the pointer already crossed is reported.
    /// And a motion report the full queue drops does not ring.
    #[test]
    fn unreported_motion_names_no_cell_and_dropped_motion_is_quiet() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.output(b"\x1b[?1000h").unwrap();
        hover(&mut window, 0, 0);
        button(&mut window, LEFT_BUTTON, true);
        hover(&mut window, 0, 1);
        window.output(b"\x1b[?1002h").unwrap();
        window
            .event(message(POINTER, 2, &[0, 8 * 256 + 200, 128]))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        assert_eq!(window.input.take_for_test(), b"\x1b[M !!\x1b[M@\"!");
        button(&mut window, LEFT_BUTTON, false);
        window.input.take_for_test();
        window.output(b"\x1b[?1003h").unwrap();
        assert!(window
            .input
            .push(&vec![b'a'; keys::MAX_INPUT_BYTES])
            .unwrap());
        hover(&mut window, 3, 3);
        assert!(!bell(&mut window), "a dropped motion report is quiet");
    }

    /// td-term's own drag keeps the pointer td-term's until it ends: a
    /// press of another button is not reported, nor is motion under 1003,
    /// nor under a mode the child set during it.
    #[test]
    fn td_terms_own_drag_keeps_the_pointer() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.output(b"Welcome").unwrap();
        hover(&mut window, 0, 0);
        button(&mut window, LEFT_BUTTON, true);
        window.output(b"\x1b[?1003h").unwrap();
        hover(&mut window, 0, 3);
        button(&mut window, RIGHT_BUTTON, true);
        button(&mut window, RIGHT_BUTTON, false);
        hover(&mut window, 0, 6);
        button(&mut window, LEFT_BUTTON, false);
        assert!(window.input.take_for_test().is_empty());
        assert_eq!(window.selected_text().unwrap().as_deref(), Some("Welcome"));
        // The drag over, buttonless motion is the child's again.
        hover(&mut window, 1, 0);
        assert_eq!(window.input.take_for_test(), b"\x1b[MC!\"");
    }

    /// A reported press is no step towards a double click: Shift-clicks
    /// either side of it at one cell are two single presses.
    #[test]
    fn a_reported_press_is_not_counted_towards_a_double_click() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.output(b"Welcome to td\x1b[?1000h").unwrap();
        hover(&mut window, 0, 2);
        modifiers(&mut window, SHIFT);
        button(&mut window, LEFT_BUTTON, true);
        button(&mut window, LEFT_BUTTON, false);
        modifiers(&mut window, 0);
        button(&mut window, LEFT_BUTTON, true);
        button(&mut window, LEFT_BUTTON, false);
        modifiers(&mut window, SHIFT);
        button(&mut window, LEFT_BUTTON, true);
        button(&mut window, LEFT_BUTTON, false);
        modifiers(&mut window, 0);
        assert_eq!(window.selected_text().unwrap(), None, "no word selected");
    }

    /// The wheel is the child's in notches of three rows, a smooth wheel's
    /// remainder carried between frames, at most ten a frame; it is
    /// td-term's under Shift and under mode 9.
    #[test]
    fn the_child_hears_the_wheel_in_bounded_notches() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        history(&mut window);
        window.output(b"\x1b[?1000h").unwrap();
        let row = 16 * 256;
        for _ in 0..3 {
            window.event(message(POINTER, 4, &[0, 0, row])).unwrap();
            window.event(message(POINTER, 5, &[])).unwrap();
        }
        assert_eq!(window.input.take_for_test(), b"\x1b[Ma!!");
        window.event(message(POINTER, 8, &[0, 20])).unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        assert_eq!(window.input.take_for_test(), b"\x1b[Ma!!".repeat(10));
        assert_eq!(window.viewport.offset(window.history()), 0);
        modifiers(&mut window, SHIFT);
        window
            .event(message(POINTER, 8, &[0, (-1i32) as u32]))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        modifiers(&mut window, 0);
        assert!(window.input.take_for_test().is_empty());
        assert_eq!(window.viewport.offset(window.history()), 3);
        press(&mut window, 107);
        // Two rows towards a notch, then a Shift wheel td-term keeps: the
        // next row starts a notch afresh. So does leaving.
        let rows = |window: &mut Window, count: u32| {
            window
                .event(message(POINTER, 4, &[0, 0, count * row]))
                .unwrap();
            window.event(message(POINTER, 5, &[])).unwrap();
        };
        rows(&mut window, 2);
        modifiers(&mut window, SHIFT);
        rows(&mut window, 3);
        modifiers(&mut window, 0);
        assert!(!window.viewport.viewing(window.history()));
        rows(&mut window, 1);
        assert!(window.input.take_for_test().is_empty());
        rows(&mut window, 1);
        window.event(message(POINTER, 1, &[8, SURFACE])).unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        window
            .event(message(POINTER, 0, &[9, SURFACE, 128, 128]))
            .unwrap();
        rows(&mut window, 1);
        assert!(window.input.take_for_test().is_empty());
        rows(&mut window, 2);
        assert_eq!(window.input.take_for_test(), b"\x1b[Ma!!");
        window.output(b"\x1b[?9h").unwrap();
        window
            .event(message(POINTER, 8, &[0, (-1i32) as u32]))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        assert!(window.input.take_for_test().is_empty());
        assert_eq!(window.viewport.offset(window.history()), 3);
    }

    #[test]
    fn a_wheel_scrolls_output_that_arrived_without_a_keystroke() {
        let (mut window, _peer) = presented();
        history(&mut window);
        window.stale = false;
        // One notch away from the operator: back three lines into history.
        window
            .event(message(POINTER, 8, &[0, (-1i32) as u32]))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        assert_eq!(window.viewport.offset(window.history()), 3);
        assert!(window.stale);
    }

    /// Copying a line the terminal wrapped gives it back as the child
    /// wrote it: no newline at the wrap, and the blank the wrap fell on
    /// kept; a line the child ended still ends in a newline, trimmed.
    #[test]
    fn a_wrapped_line_is_copied_whole() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        let columns = window.model.as_ref().unwrap().columns();
        let mut text = "x".repeat(columns - 1);
        text.push_str(" tail\r\nnext  ");
        window.output(text.as_bytes()).unwrap();
        window.selection = selection((0, 0), (2, 3));
        let mut expected = "x".repeat(columns - 1);
        expected.push_str(" tail\nnext");
        assert_eq!(window.selected_text().unwrap(), Some(expected));
        // A triple press takes the whole wrapped line.
        window.selection = None;
        window.clock = 10_000;
        for _ in 0..3 {
            let clock = window.clock;
            press_at(&mut window, 2, clock, 9, true);
            press_at(&mut window, 2, clock, 9, false);
        }
        let mut whole = "x".repeat(columns - 1);
        whole.push_str(" tail");
        assert_eq!(window.selected_text().unwrap(), Some(whole));
        // A selection that ends on the wrapped row is trimmed there.
        window.selection = selection((0, 0), (0, columns - 1));
        assert_eq!(
            window.selected_text().unwrap(),
            Some("x".repeat(columns - 1))
        );
    }

    /// Edge blanks kept for a wrapped row's sake do not end the text, and a
    /// selection of wrapped blanks alone copies nothing.
    #[test]
    fn a_copy_ends_with_no_wrapped_blanks() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        let columns = window.model.as_ref().unwrap().columns();
        let mut text = "x".repeat(columns - 2);
        text.push_str(&" ".repeat(columns + 4));
        text.push_str("end");
        window.output(text.as_bytes()).unwrap();
        window.selection = selection((0, 0), (1, 1));
        assert_eq!(
            window.selected_text().unwrap(),
            Some("x".repeat(columns - 2))
        );
        window.selection = selection((0, columns - 1), (1, 2));
        assert_eq!(window.selected_text().unwrap(), None);
    }

    /// Scrolled back, a line wrapped from history onto the screen copies
    /// whole across the split.
    #[test]
    fn a_wrapped_line_across_the_history_split_is_copied_whole() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        let (rows, columns) = {
            let model = window.model.as_ref().unwrap();
            (model.rows(), model.columns())
        };
        let mut text = "y".repeat(columns);
        text.push_str("zz");
        text.push_str(&"\r\n".repeat(rows - 1));
        window.output(text.as_bytes()).unwrap();
        // The y row is the newest in history, its rest the screen's first.
        modifiers(&mut window, SHIFT);
        press(&mut window, 104);
        modifiers(&mut window, 0);
        let back = window.viewport.offset(window.history());
        assert!(back > 0);
        let split = back - 1;
        window.selection = selection((split, 0), (split + 1, 1));
        let mut expected = "y".repeat(columns);
        expected.push_str("zz");
        assert_eq!(window.selected_text().unwrap(), Some(expected));
    }

    /// Types `text` as plain key presses (lowercase letters and spaces).
    fn type_text(window: &mut Window, text: &str) {
        for letter in text.chars() {
            let key = match letter {
                ' ' => 57,
                'a' => 30,
                'e' => 18,
                'f' => 33,
                'n' => 49,
                'o' => 24,
                'r' => 19,
                't' => 20,
                'w' => 17,
                other => panic!("no key for {other:?}"),
            };
            press(window, key);
        }
    }

    fn chord(window: &mut Window, mask: u32, key: u32) {
        modifiers(window, mask);
        press(window, key);
        modifiers(window, 0);
    }

    /// C-S-r opens a search that takes every key: typing finds the newest
    /// match and scrolls it into view, selected; C-r and C-s step older and
    /// newer; Backspace shortens the query; nothing reaches the child.
    /// Escape puts the view and selection back.
    #[test]
    fn a_search_finds_steps_and_cancels() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.output(b"one foo\r\n").unwrap();
        history(&mut window);
        window.output(b"two foo\r\n").unwrap();
        let before = selection((0, 0), (0, 1));
        window.selection = before;
        chord(&mut window, CONTROL | SHIFT, 19);
        assert!(window.search.is_some());
        assert_eq!(window.selection, None, "the search shows its own");
        type_text(&mut window, "foo");
        assert!(window.input.take_for_test().is_empty());
        let newest = window.search_selection().unwrap();
        assert_eq!(status(&window).as_deref(), Some("search: foo"));
        assert!(!window.viewport.viewing(window.history()));
        chord(&mut window, CONTROL, 19);
        assert!(window.viewport.viewing(window.history()), "scrolled to it");
        let older = window.search_selection().unwrap();
        assert_ne!(older, newest);
        window.selection = Some(older);
        assert_eq!(window.selected_text().unwrap().as_deref(), Some("foo"));
        window.selection = None;
        // No older match: the bell, and the match stays.
        chord(&mut window, CONTROL, 19);
        assert!(bell(&mut window));
        assert_eq!(window.search_selection(), Some(older));
        chord(&mut window, CONTROL, 31);
        assert_eq!(window.search_selection(), Some(newest));
        press(&mut window, 14);
        assert_eq!(window.search.as_ref().unwrap().query, "fo");
        assert_eq!(
            window.search_selection().map(|found| found.anchor),
            Some(newest.anchor),
            "still matches where it was"
        );
        type_text(&mut window, "w");
        assert_eq!(status(&window).as_deref(), Some("search (no match): fow"));
        // A dead end, then Backspace: the search picks up from the last
        // match it showed, here an older one, not the newest.
        press(&mut window, 14);
        chord(&mut window, CONTROL, 19);
        let shown = window.search_selection().unwrap();
        assert_ne!(shown.anchor, newest.anchor);
        type_text(&mut window, "w");
        assert!(window.search.as_ref().unwrap().found.is_none());
        press(&mut window, 14);
        assert_eq!(
            window.search_selection().map(|found| found.anchor),
            Some(shown.anchor)
        );
        press(&mut window, 1);
        assert!(window.search.is_none());
        assert_eq!(window.selection, before);
        assert!(!window.viewport.viewing(window.history()));
        assert!(window.input.take_for_test().is_empty());
        // Cancelled while it shows an older match, the view goes back.
        chord(&mut window, CONTROL | SHIFT, 19);
        type_text(&mut window, "one");
        assert!(window.viewport.viewing(window.history()));
        chord(&mut window, CONTROL, 34);
        assert!(window.search.is_none());
        assert!(!window.viewport.viewing(window.history()));
        // C-c cancels too.
        chord(&mut window, CONTROL | SHIFT, 19);
        chord(&mut window, CONTROL, 46);
        assert!(window.search.is_none());
        assert!(window.input.take_for_test().is_empty());
    }

    fn status(window: &Window) -> Option<String> {
        window.search_status().map(|(text, _)| text)
    }

    /// A match on the view's last row is not under the search line, which
    /// moves to the first row; one that wraps past the view's last row is
    /// scrolled on whole; the frame shows the match, not the selection
    /// the search began with.
    #[test]
    fn a_search_shows_its_whole_match() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        let (rows, columns) = {
            let model = window.model.as_ref().unwrap();
            (model.rows(), model.columns())
        };
        history(&mut window);
        let mut text = "\r\n".repeat(rows);
        text.push_str("$ foo");
        window.output(text.as_bytes()).unwrap();
        window.selection = selection((0, 0), (0, 1));
        chord(&mut window, CONTROL | SHIFT, 19);
        type_text(&mut window, "foo");
        let found = window.search_selection().unwrap();
        assert_eq!(found.anchor, (rows - 1, 2));
        assert_eq!(window.shown_selection(), Some(found));
        assert_eq!(
            window.search_status().map(|(_, edge)| edge),
            Some(render::Edge::Top)
        );
        press(&mut window, 1);
        assert_eq!(window.shown_selection(), selection((0, 0), (0, 1)));
        // Scrolled back so a wrapped match's first row is the view's last.
        let mut text = "\r\n".to_string();
        text.push_str(&"x".repeat(columns - 2));
        text.push_str("fooo\r\n");
        text.push_str(&"\r\n".repeat(rows));
        window.output(text.as_bytes()).unwrap();
        modifiers(&mut window, SHIFT);
        press(&mut window, 104);
        modifiers(&mut window, 0);
        chord(&mut window, CONTROL | SHIFT, 19);
        type_text(&mut window, "foo");
        let found = window.search_selection().unwrap();
        // With the match's first row the view's last, a refined query
        // brings its second row on too.
        let history = window.history();
        window
            .viewport
            .by_lines((rows - 1 - found.anchor.0) as i32, history);
        assert_eq!(window.search_selection().unwrap().anchor.0, rows - 1);
        type_text(&mut window, "o");
        let found = window.search_selection().unwrap();
        assert_eq!(found.extent.0, found.anchor.0 + 1, "both rows on the view");
        assert_eq!(status(&window).as_deref(), Some("search: fooo"));
        // Moved so only its first row is on the view, the match is drawn
        // cut at the view's edge; moved off, it is still found.
        let history = window.history();
        let back = rows - 1 - found.anchor.0;
        window.viewport.by_lines(back as i32, history);
        assert_eq!(
            window.search_selection(),
            selection((rows - 1, columns - 2), (rows - 1, columns - 1))
        );
        window.viewport.by_lines(rows as i32, history);
        assert_eq!(window.search_selection(), None);
        assert_eq!(status(&window).as_deref(), Some("search: fooo"));
        // Committed, the whole match is brought back and selected.
        press(&mut window, 96);
        assert_eq!(window.selected_text().unwrap().as_deref(), Some("fooo"));
    }

    /// A match goes when its text does: a clear renumbering history, the
    /// child rewriting its cells, or the other screen coming up. A press
    /// ends a search where the view is.
    #[test]
    fn a_search_drops_a_match_the_text_no_longer_holds() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.output(b"foo\r\n").unwrap();
        chord(&mut window, CONTROL | SHIFT, 19);
        type_text(&mut window, "foo");
        assert!(window.search_selection().is_some());
        window.output(b"\x1b[Hbar").unwrap();
        assert!(window.search_selection().is_none(), "rewritten");
        assert_eq!(status(&window).as_deref(), Some("search (no match): foo"));
        press(&mut window, 1);
        window.output(b"\x1b[2J\x1b[Hfoo\r\n").unwrap();
        history(&mut window);
        chord(&mut window, CONTROL | SHIFT, 19);
        type_text(&mut window, "foo");
        assert!(window.search.as_ref().unwrap().found.is_some());
        window.output(b"\x1b[3J").unwrap();
        assert!(
            window.search.as_ref().unwrap().found.is_none(),
            "renumbered"
        );
        assert!(window.search.as_ref().unwrap().last.is_none());
        press(&mut window, 1);
        window.output(b"\x1b[2J\x1b[3J\x1b[Hfoo").unwrap();
        chord(&mut window, CONTROL | SHIFT, 19);
        type_text(&mut window, "foo");
        assert!(window.search.as_ref().unwrap().found.is_some());
        // The same text at the same place on the other screen is other text.
        window.output(b"\x1b[?1049h\x1b[Hfoo").unwrap();
        assert!(
            window.search.as_ref().unwrap().found.is_none(),
            "other screen"
        );
        assert!(window.search.as_ref().unwrap().last.is_none());
        // On the alternate screen the match is shown at the live view.
        window.output(b"\x1b[Hfoo").unwrap();
        modifiers(&mut window, SHIFT);
        window
            .event(message(POINTER, 8, &[0, (-1i32) as u32]))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        modifiers(&mut window, 0);
        chord(&mut window, CONTROL, 19);
        assert!(!window.viewport.viewing(window.history()));
        assert_eq!(window.search_selection(), selection((0, 0), (0, 2)));
        // A press ends it, the view staying.
        hover(&mut window, 1, 1);
        button(&mut window, LEFT_BUTTON, true);
        assert!(window.search.is_none());
        button(&mut window, LEFT_BUTTON, false);
    }

    /// A key held in a search repeats into the query, never to the child;
    /// a dead end stops a held step.
    #[test]
    fn a_held_search_key_repeats_into_the_query() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.client.input_mut().timing(25, 600, 0).unwrap();
        window.output(b"foo\r\nfoo\r\n").unwrap();
        chord(&mut window, CONTROL | SHIFT, 19);
        type_text(&mut window, "f");
        // 'o' held: the press and one repeat.
        window.event(message(KEYBOARD, 3, &[9, 0, 24, 1])).unwrap();
        let now = window.clock;
        window.end_turn(now + 700, true).unwrap();
        assert_eq!(window.search.as_ref().unwrap().query, "foo");
        assert!(window.input.take_for_test().is_empty());
        window.event(message(KEYBOARD, 3, &[10, 0, 24, 0])).unwrap();
        // C-r held: the press steps to the older match, the repeat finds
        // none further, rings, and stops.
        modifiers(&mut window, CONTROL);
        window.event(message(KEYBOARD, 3, &[11, 0, 19, 1])).unwrap();
        let newest = window.search.as_ref().unwrap().found;
        let now = window.clock;
        window.end_turn(now + 700, true).unwrap();
        assert_eq!(window.search.as_ref().unwrap().found, newest);
        assert!(bell(&mut window));
        window.end_turn(now + 800, true).unwrap();
        assert!(!bell(&mut window), "the repeat stopped");
        assert!(window.input.take_for_test().is_empty());
        assert_eq!(window.search.as_ref().unwrap().query, "foo");
    }

    /// On the alternate screen a match is shown only at the live view;
    /// a resize that moves its cells drops it; Return with no match keeps
    /// the selection the search began with.
    #[test]
    fn a_search_match_is_shown_only_where_it_is() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        history(&mut window);
        window.output(b"\x1b[?1049h\x1b[Hfoo").unwrap();
        chord(&mut window, CONTROL | SHIFT, 19);
        type_text(&mut window, "foo");
        assert_eq!(window.search_selection(), selection((0, 0), (0, 2)));
        let history = window.history();
        window.viewport.by_lines(3, history);
        assert_eq!(window.search_selection(), None);
        assert_eq!(
            window.search_status().map(|(_, edge)| edge),
            Some(render::Edge::Bottom)
        );
        press(&mut window, 1);
        window.output(b"\x1b[?1049l\x1b[2J\x1b[Hxxxxxxfoo").unwrap();
        chord(&mut window, CONTROL | SHIFT, 19);
        type_text(&mut window, "foo");
        assert!(window.search.as_ref().unwrap().found.is_some());
        let (width, height) = (window.cell.0 * 8, window.cell.1 * 4);
        window.adopt(Size { width, height }).unwrap();
        assert!(window.search.as_ref().unwrap().found.is_none(), "reflowed");
        let before = selection((1, 0), (1, 1));
        window.search.as_mut().unwrap().selection = before;
        press(&mut window, 28);
        assert!(window.search.is_none());
        assert_eq!(window.selection, before);
    }

    /// A query past its bound rings rather than growing.
    #[test]
    fn a_search_query_is_bounded() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        chord(&mut window, CONTROL | SHIFT, 19);
        window.search.as_mut().unwrap().query = "o".repeat(td_ui::vt::MAX_QUERY);
        type_text(&mut window, "o");
        assert!(bell(&mut window));
        assert_eq!(
            window.search.as_ref().unwrap().query.len(),
            td_ui::vt::MAX_QUERY
        );
    }

    /// Return ends a search with its match selected and made the primary
    /// selection; outside a search C-r is the child's.
    #[test]
    fn a_committed_search_selects_its_match() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.output(b"to an end\r\n").unwrap();
        chord(&mut window, CONTROL | SHIFT, 19);
        type_text(&mut window, "an e");
        press(&mut window, 28);
        assert!(window.search.is_none());
        assert_eq!(window.selected_text().unwrap().as_deref(), Some("an e"));
        assert_eq!(window.board.primary.text.as_deref(), Some("an e"));
        chord(&mut window, CONTROL, 19);
        assert_eq!(window.input.take_for_test(), b"\x12");
    }

    #[test]
    fn a_pointer_drag_selects_and_the_copy_chord_offers_it() {
        let (mut window, peer) = presented();
        focus(&mut window, 5);
        window.output(b"Welcome to td").unwrap();
        let fixed = |cells: u32, size: u32| cells * size * 256 + 128;
        window
            .event(message(
                POINTER,
                0,
                &[1, SURFACE, fixed(0, 8), fixed(0, 16)],
            ))
            .unwrap();
        window
            .event(message(POINTER, 3, &[2, 0, LEFT_BUTTON, 1]))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        window
            .event(message(POINTER, 2, &[0, fixed(6, 8), fixed(0, 16)]))
            .unwrap();
        window
            .event(message(POINTER, 3, &[3, 0, LEFT_BUTTON, 0]))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        assert_eq!(window.selected_text().unwrap().as_deref(), Some("Welcome"));
        peer::drain(&peer).unwrap();
        modifiers(&mut window, SHIFT | CONTROL);
        press(&mut window, 46);
        assert!(
            window.input.take_for_test().is_empty(),
            "the chord is the terminal's"
        );
        let (requests, _) = peer::drain(&peer).unwrap();
        assert!(
            requests.iter().any(|m| m.object == DEVICE && m.opcode == 1),
            "set_selection"
        );
        assert_eq!(window.board.clipboard.text.as_deref(), Some("Welcome"));
        assert!(window.selection.is_some(), "a copy keeps the selection");
        assert!(window.board.sync.is_none(), "no proof, no sync");
    }

    /// A pointer at a column of the first row.
    fn point(window: &mut Window, column: u32) {
        window
            .event(message(POINTER, 2, &[0, column * 8 * 256 + 128, 128]))
            .unwrap();
    }

    /// A left press, or release, and the frame that closes it. Each press
    /// comes after the last one's multi-click window, so it is a single.
    fn click(window: &mut Window, pressed: bool) {
        if pressed {
            window.clock += MULTI_CLICK_MS + 1;
        }
        window
            .event(message(
                POINTER,
                3,
                &[2, 0, LEFT_BUTTON, u32::from(pressed)],
            ))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
    }

    /// The model drawn and on screen.
    fn shown(window: &mut Window) {
        window.draw().unwrap();
        complete(window);
    }

    /// A bell is drawn: the next frame inverts the ring at the surface's
    /// edge and takes the bell, frames until the flash is over keep the
    /// ring, a bell meanwhile puts the end forward, and the first frame
    /// after it is drawn without.
    #[test]
    fn a_bell_rings_in_the_frames_until_its_flash_is_over() {
        let (mut window, _peer) = presented();
        // A pixel on the top edge, over a blank cell in every frame here.
        let edge = |window: &Window| window.client.pixels().get(1280..1284).map(<[u8]>::to_vec);
        let plain = edge(&window);
        window.clock = 1000;
        window.output(b"\x07").unwrap();
        shown(&mut window);
        let rung = edge(&window);
        assert_ne!(rung, plain, "the next frame rings");
        assert!(!bell(&mut window), "and took the bell");
        assert_eq!(
            window.next_wait(1000 + BELL_FLASH_MS - 10),
            10,
            "the turn wakes as the flash ends"
        );
        window.clock = 1040;
        window.end_flash(1040);
        window.output(b"y").unwrap();
        shown(&mut window);
        assert_eq!(edge(&window), rung, "a frame in the flash keeps the ring");
        window.clock = 1050;
        window.end_flash(1050);
        window.output(b"\x07x").unwrap();
        shown(&mut window);
        assert_eq!(edge(&window), rung, "a bell in the flash");
        window.end_flash(1000 + BELL_FLASH_MS);
        assert!(window.flash.is_some(), "put forward");
        window.end_flash(1050 + BELL_FLASH_MS);
        assert!(window.flash.is_none() && window.stale);
        shown(&mut window);
        assert_eq!(edge(&window), plain, "the flash over");
    }

    /// Control alone over a link rules it, and nothing else does: the
    /// frame wanted carries the link's cells, so holding Control, letting
    /// it go, or moving off the link needs a frame though the model is
    /// unchanged. The drawn frame rules exactly those cells. No link is
    /// ruled where a press would not follow it: under mouse reporting, or
    /// with a search open.
    #[test]
    fn control_over_a_link_rules_it() {
        const NUM_LOCK: u32 = 16;
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        let mut text = String::from("see https://e.example/x now\r\n");
        text.push_str(&"x".repeat(60));
        text.push_str(" https://e.example/yz");
        window.output(text.as_bytes()).unwrap();
        shown(&mut window);
        let link = Some(render::LinkSpan {
            row: 0,
            start: 4,
            end: 23,
        });
        let hovered = |window: &Window| window.wanted().and_then(|drawn| drawn.link);
        assert_eq!(hovered(&window), None, "not over the surface");
        window
            .event(message(POINTER, 0, &[1, SURFACE, 10 * 8 * 256 + 128, 128]))
            .unwrap();
        assert_eq!(hovered(&window), None, "no Control");
        modifiers(&mut window, CONTROL | NUM_LOCK);
        assert_eq!(hovered(&window), link);
        assert!(!window.stale, "the model is unchanged");
        assert_ne!(window.drawn, window.wanted(), "but the frame is not");
        shown(&mut window);
        let (width, height) = window.cell;
        let row = height - 2;
        let lit = |window: &Window, column: usize| {
            (0..width).all(|x| {
                let at = (row * 640 + column * width + x) * 4;
                window
                    .client
                    .pixels()
                    .get(at..at + 3)
                    .is_some_and(|pixel| pixel == [255, 255, 255])
            })
        };
        assert!(lit(&window, 4) && lit(&window, 22));
        assert!(!lit(&window, 3) && !lit(&window, 23));
        for other in [SHIFT, ALT] {
            modifiers(&mut window, CONTROL | other);
            assert_eq!(hovered(&window), None, "Control alone");
        }
        modifiers(&mut window, CONTROL);
        point(&mut window, 2);
        assert_eq!(hovered(&window), None, "off the link");
        // A link that runs to the row's end.
        window
            .event(message(
                POINTER,
                2,
                &[0, 70 * 8 * 256 + 128, 16 * 256 + 128],
            ))
            .unwrap();
        assert_eq!(
            hovered(&window),
            Some(render::LinkSpan {
                row: 1,
                start: 61,
                end: 80,
            })
        );
        point(&mut window, 22);
        assert_eq!(hovered(&window), link);
        window.output(b"\x1b[?1000h").unwrap();
        assert_eq!(hovered(&window), None, "the child takes the press");
        window.output(b"\x1b[?1000l").unwrap();
        chord(&mut window, CONTROL | SHIFT, 19);
        modifiers(&mut window, CONTROL);
        assert_eq!(hovered(&window), None, "a search is open");
        modifiers(&mut window, 0);
        press(&mut window, 1);
        modifiers(&mut window, CONTROL);
        assert_eq!(hovered(&window), link);
        window.event(message(KEYBOARD, 2, &[6, SURFACE])).unwrap();
        assert_eq!(hovered(&window), None, "focus went");
        focus(&mut window, 7);
        modifiers(&mut window, CONTROL);
        window.event(message(POINTER, 1, &[2, SURFACE])).unwrap();
        assert_eq!(hovered(&window), None, "left the surface");
        modifiers(&mut window, 0);
    }

    /// A resize reflows the cells under the pointer: the frame it draws
    /// rules the link as the reflowed row holds it.
    #[test]
    fn a_resize_rules_the_link_where_the_reflow_put_it() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.output(b"see https://e.example/x now").unwrap();
        shown(&mut window);
        window
            .event(message(POINTER, 0, &[1, SURFACE, 10 * 8 * 256 + 128, 128]))
            .unwrap();
        modifiers(&mut window, CONTROL);
        shown(&mut window);
        configure(&mut window, 16 * 8, 320, true);
        window.draw().unwrap();
        assert_eq!(
            window.drawn.and_then(|drawn| drawn.link),
            Some(render::LinkSpan {
                row: 0,
                start: 4,
                end: 16,
            })
        );
        modifiers(&mut window, 0);
    }

    /// A frame drawn only because the hovered link changed shows the same
    /// model: a Control-press before its callback still follows the link.
    #[test]
    fn a_control_press_follows_a_link_while_its_rule_is_in_flight() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.browser = Some("/nonexistent/td-term-browser".into());
        window.output(b"see https://e.example/x now").unwrap();
        shown(&mut window);
        window
            .event(message(POINTER, 0, &[1, SURFACE, 10 * 8 * 256 + 128, 128]))
            .unwrap();
        modifiers(&mut window, CONTROL);
        window.draw().unwrap();
        assert!(window.frame.as_ref().is_some_and(|frame| !frame.presented));
        click(&mut window, true);
        assert!(bell(&mut window), "followed, and no browser rang");
        click(&mut window, false);
        // A frame for a changed model is not what the screen showed.
        complete(&mut window);
        window.output(b"!").unwrap();
        window.draw().unwrap();
        click(&mut window, true);
        click(&mut window, false);
        assert!(!bell(&mut window), "a plain press");
        modifiers(&mut window, 0);
    }

    /// A frame that could not be submitted, every buffer held, leaves
    /// the bell for the first that is, however long that takes.
    #[test]
    fn a_bell_waits_for_a_frame_that_presents() {
        let (mut window, _peer) = presented();
        let edge = |window: &Window| window.client.pixels().get(1280..1284).map(<[u8]>::to_vec);
        let plain = edge(&window);
        let mut held = Vec::new();
        for _ in 0..3 {
            window.stale = true;
            window.draw().unwrap();
            let callback = window.client.frame_callback().unwrap();
            window.event(message(callback, 0, &[0])).unwrap();
            window.event(message(DISPLAY, 1, &[callback])).unwrap();
            held.push(window.client.presented().unwrap());
        }
        window.clock = 1000;
        window.output(b"\x07").unwrap();
        window.draw().unwrap();
        assert!(window.flash.is_none(), "no frame, no flash");
        window.end_flash(1000 + BELL_FLASH_MS);
        window.event(message(held[0], 0, &[])).unwrap();
        window.clock = 2000;
        window.draw().unwrap();
        assert_ne!(edge(&window), plain, "the frame that presents rings");
        assert_eq!(window.flash, Some(2000 + BELL_FLASH_MS));
        assert!(!bell(&mut window));
    }

    fn bell(window: &mut Window) -> bool {
        window.model.as_mut().unwrap().take_bell()
    }

    /// A Control-press over a link opens it through td-ui's opener (here a
    /// browser that cannot start, so the bell rings) and selects nothing:
    /// the selection stays through the press, its drag and its release.
    /// Control off a link, and a press without Control on one, are plain
    /// presses, which clear the selection.
    #[test]
    fn a_control_press_follows_a_link_and_is_otherwise_a_selection() {
        const NUM_LOCK: u32 = 16;
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.browser = Some("/nonexistent/td-term-browser".into());
        window.output(b"see https://e.example/x now").unwrap();
        shown(&mut window);
        let before = Some(render::Selection {
            anchor: (1, 0),
            extent: (1, 3),
        });
        window.selection = before;
        window
            .event(message(POINTER, 0, &[1, SURFACE, 10 * 8 * 256 + 128, 128]))
            .unwrap();
        modifiers(&mut window, CONTROL | NUM_LOCK);
        click(&mut window, true);
        assert!(bell(&mut window), "no browser, and no bell");
        assert_eq!(window.selection, before, "the followed press selected");
        point(&mut window, 14);
        window.event(message(POINTER, 5, &[])).unwrap();
        click(&mut window, false);
        assert_eq!(window.selection, before, "its drag or release selected");
        assert!(!bell(&mut window));

        // Control off the link is a plain press.
        shown(&mut window);
        point(&mut window, 1);
        click(&mut window, true);
        click(&mut window, false);
        assert_eq!(window.selection, None);
        // Without Control a press on the link is plain.
        modifiers(&mut window, 0);
        window.selection = before;
        shown(&mut window);
        point(&mut window, 10);
        click(&mut window, true);
        click(&mut window, false);
        assert_eq!(window.selection, None);
        assert!(!bell(&mut window));
        // Nor with Shift or Alt beside Control: the press is Control's alone.
        for (other, column) in [(SHIFT, 11), (ALT, 12)] {
            modifiers(&mut window, CONTROL | other);
            window.selection = before;
            shown(&mut window);
            point(&mut window, column);
            click(&mut window, true);
            click(&mut window, false);
            assert_eq!(window.selection, None);
            assert!(!bell(&mut window));
        }
        point(&mut window, 10);

        // The link is read at the press, from the screen then: output before
        // the frame closes cannot change what opens. A press and release in
        // one frame leave nothing held.
        modifiers(&mut window, CONTROL);
        window.selection = before;
        shown(&mut window);
        window
            .event(message(POINTER, 3, &[2, 0, LEFT_BUTTON, 1]))
            .unwrap();
        assert_eq!(window.drag.link.as_deref(), Some("https://e.example/x"));
        window
            .model
            .as_mut()
            .unwrap()
            .feed(b"\rsee https://z.example/y now");
        window.stale = true;
        assert_eq!(
            window.link_at((10 * 8 * 256 + 128, 128)).as_deref(),
            Some("https://z.example/y")
        );
        click(&mut window, false);
        assert!(bell(&mut window));
        assert_eq!(window.selection, before);
        assert!(!window.drag.following, "a released press is still held");

        // A screen not yet drawn is not what was seen: the press is plain.
        assert!(window.stale);
        click(&mut window, true);
        click(&mut window, false);
        assert!(!bell(&mut window));
        assert_eq!(window.selection, None);

        // Nor is one committed whose callback has not come: the old picture
        // may still be up. Once it has, the press follows.
        window.draw().unwrap();
        assert!(!window.stale);
        click(&mut window, true);
        click(&mut window, false);
        assert!(!bell(&mut window));
        complete(&mut window);
        click(&mut window, true);
        click(&mut window, false);
        assert!(bell(&mut window));

        // Nor is a model a resize reflowed whose frames never presented,
        // though it is back at the size on screen: the narrow reflow may
        // have cut the link the picture still shows whole.
        shown(&mut window);
        // No selection, whose clearing would mark the change by itself.
        window.selection = None;
        for (width, height) in [(160, 320), (640, 320)] {
            window.adopt(Size { width, height }).unwrap();
        }
        assert_eq!(window.drawn.map(|drawn| drawn.size), window.adopted);
        click(&mut window, true);
        click(&mut window, false);
        assert!(!bell(&mut window));
        shown(&mut window);
        click(&mut window, true);
        click(&mut window, false);
        assert!(bell(&mut window));

        // Past the drawn grid is no link, though a selection's clamp would
        // reach the edge cell.
        let mut edge = Terminal::new(1, 20).unwrap();
        edge.feed(b"https://e.example/ab");
        window.model = Some(edge);
        window.cells = Some((1, 20));
        let at = |column: i32| (column * 8 * 256 + 128, 128);
        assert_eq!(
            window.link_at(at(19)).as_deref(),
            Some("https://e.example/ab")
        );
        assert_eq!(window.link_at(at(20)), None);
        // A model wider than the drawn grid is read only as far as it is drawn.
        window.cells = Some((1, 15));
        assert_eq!(window.link_at(at(14)).as_deref(), Some("https://e.examp"));
        window.cells = Some((1, 20));
        assert_eq!(window.link_at((at(5).0, 16 * 256 + 128)), None);
        assert_eq!(window.link_at((-256, 128)), None);
    }

    /// A left press or release at `serial` at a column of the first row,
    /// at `clock`, and the frame that closes it.
    fn press_at(window: &mut Window, column: u32, clock: u64, serial: u32, pressed: bool) {
        window.clock = clock;
        point(window, column);
        window
            .event(message(
                POINTER,
                3,
                &[serial, 0, LEFT_BUTTON, u32::from(pressed)],
            ))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
    }

    fn selection(anchor: (usize, usize), extent: (usize, usize)) -> Option<render::Selection> {
        Some(render::Selection { anchor, extent })
    }

    /// A second press at a cell within the multi-click window selects the
    /// word under it, a third its row, and a fourth starts over; a slow
    /// press, or one at another cell, is a single.
    #[test]
    fn a_double_press_selects_the_word_and_a_triple_its_row() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.output(b"ls -la /tmp:foo").unwrap();
        let columns = usize::from(window.cells.unwrap().1);
        press_at(&mut window, 8, 1000, 2, true);
        press_at(&mut window, 8, 1050, 3, false);
        assert_eq!(window.selection, None, "a single selects nothing");
        press_at(&mut window, 8, 1200, 4, true);
        assert_eq!(window.selection, selection((0, 7), (0, 10)));
        press_at(&mut window, 8, 1250, 5, false);
        assert_eq!(window.selected_text().unwrap().as_deref(), Some("/tmp"));
        press_at(&mut window, 8, 1400, 6, true);
        press_at(&mut window, 8, 1450, 7, false);
        assert_eq!(window.selection, selection((0, 0), (0, columns - 1)));
        assert_eq!(
            window.selected_text().unwrap().as_deref(),
            Some("ls -la /tmp:foo")
        );
        press_at(&mut window, 8, 1600, 8, true);
        press_at(&mut window, 8, 1650, 9, false);
        assert_eq!(window.selection, None, "a fourth starts over");
        // Too slow for a double.
        press_at(&mut window, 8, 3000, 10, true);
        press_at(&mut window, 8, 3050, 11, false);
        press_at(&mut window, 8, 3600, 12, true);
        press_at(&mut window, 8, 3650, 13, false);
        assert_eq!(window.selection, None);
        // Another cell is another gesture.
        press_at(&mut window, 13, 3700, 14, true);
        press_at(&mut window, 13, 3720, 15, false);
        assert_eq!(window.selection, None);
        press_at(&mut window, 13, 3800, 16, true);
        press_at(&mut window, 13, 3820, 17, false);
        assert_eq!(window.selection, selection((0, 12), (0, 14)));
        // Another button's press between two ends the run.
        press_at(&mut window, 13, 5000, 18, true);
        press_at(&mut window, 13, 5020, 19, false);
        window
            .event(message(POINTER, 3, &[20, 0, MIDDLE_BUTTON, 1]))
            .unwrap();
        window
            .event(message(POINTER, 3, &[21, 0, MIDDLE_BUTTON, 0]))
            .unwrap();
        press_at(&mut window, 13, 5100, 22, true);
        press_at(&mut window, 13, 5120, 23, false);
        assert_eq!(window.selection, None, "a single, not a double");
    }

    /// A followed link press counts toward no gesture: a plain press at
    /// its cell right after is a single, not a double.
    #[test]
    fn a_followed_link_press_starts_no_gesture() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.browser = Some("/nonexistent/td-term-browser".into());
        window.output(b"see https://e.example/x now").unwrap();
        shown(&mut window);
        modifiers(&mut window, CONTROL);
        press_at(&mut window, 10, 1000, 2, true);
        press_at(&mut window, 10, 1050, 3, false);
        assert!(bell(&mut window), "followed");
        modifiers(&mut window, 0);
        shown(&mut window);
        press_at(&mut window, 10, 1100, 4, true);
        press_at(&mut window, 10, 1150, 5, false);
        assert_eq!(window.selection, None);
    }

    /// td's own compositor offers no primary-selection manager: a drag's
    /// release offers nothing and a middle press does nothing, and neither
    /// ends the terminal.
    #[test]
    fn without_a_primary_selection_a_release_and_a_middle_press_do_nothing() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        theirs
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let options = Options {
            socket: None,
            profile: Profile::Desktop,
            working_directory: None,
            command: Vec::new(),
            font_size: None,
        };
        let mut window = Window::new(ours, options, "/run/wayland-test".into()).unwrap();
        for event in [
            global(1, "wl_compositor", 4),
            global(2, "wl_shm", 1),
            global(3, "xdg_wm_base", 1),
            global(4, "wl_seat", 7),
            global(5, "wl_data_device_manager", 3),
        ] {
            window.event(event).unwrap();
        }
        window.event(message(SYNC, 0, &[0])).unwrap();
        window.event(message(DISPLAY, 1, &[SYNC])).unwrap();
        window.event(message(SHM, 0, &[1])).unwrap();
        window.event(message(SEAT, 0, &[3])).unwrap();
        assert!(!window.client.primary());
        let pointer = window.client.pointer().unwrap();
        configure(&mut window, 640, 320, true);
        window.draw().unwrap();
        window.output(b"Welcome").unwrap();
        peer::drain(&theirs).unwrap();
        let at = |column: u32| column * 8 * 256 + 128;
        for event in [
            message(pointer, 0, &[1, SURFACE, at(0), 128]),
            message(pointer, 3, &[2, 0, LEFT_BUTTON, 1]),
            message(pointer, 5, &[]),
            message(pointer, 2, &[0, at(6), 128]),
            message(pointer, 3, &[3, 0, LEFT_BUTTON, 0]),
            message(pointer, 5, &[]),
            message(pointer, 3, &[4, 0, MIDDLE_BUTTON, 1]),
            message(pointer, 5, &[]),
        ] {
            window.event(event).unwrap();
        }
        assert_eq!(window.selected_text().unwrap().as_deref(), Some("Welcome"));
        assert!(window.board.primary.text.is_none());
        assert!(window.board.incoming.is_none());
        assert!(peer::drain(&theirs).unwrap().0.is_empty());
    }

    /// A drag after a double press extends a word at a time, and the
    /// pressed word stays selected whichever way it goes.
    #[test]
    fn a_double_press_drags_by_words() {
        let (mut window, _peer) = presented();
        window.output(b"one two three").unwrap();
        press_at(&mut window, 5, 1000, 2, true);
        press_at(&mut window, 5, 1050, 3, false);
        press_at(&mut window, 5, 1100, 4, true);
        point(&mut window, 9);
        window.event(message(POINTER, 5, &[])).unwrap();
        assert_eq!(window.selection, selection((0, 4), (0, 12)));
        point(&mut window, 1);
        window.event(message(POINTER, 5, &[])).unwrap();
        assert_eq!(window.selection, selection((0, 6), (0, 0)));
        press_at(&mut window, 1, 1300, 5, false);
        assert_eq!(window.selected_text().unwrap().as_deref(), Some("one two"));
    }

    /// The selection a release finishes becomes the primary selection at
    /// the release's serial, and a send writes its text. A click, which
    /// selects nothing, offers nothing.
    #[test]
    fn a_release_makes_the_selection_primary_and_its_send_writes_it() {
        let (mut window, peer) = presented();
        window.output(b"Welcome to td").unwrap();
        press_at(&mut window, 2, 1000, 2, true);
        press_at(&mut window, 2, 1050, 3, false);
        let (requests, _) = peer::drain(&peer).unwrap();
        assert!(!requests.iter().any(|m| m.object == PRIMARY_DEVICE));
        press_at(&mut window, 0, 2000, 4, true);
        point(&mut window, 6);
        window.event(message(POINTER, 5, &[])).unwrap();
        let (requests, _) = peer::drain(&peer).unwrap();
        assert!(
            !requests.iter().any(|m| m.object == PRIMARY_DEVICE),
            "not while the button is held"
        );
        press_at(&mut window, 6, 2100, 77, false);
        let (requests, _) = peer::drain(&peer).unwrap();
        let source = window.client.primary_source().unwrap();
        assert_eq!(
            requests,
            [
                message(PRIMARY_MANAGER, 0, &[source]),
                text(source, 0, td_ui::data::UTF8),
                text(source, 0, td_ui::data::PLAIN),
                message(PRIMARY_DEVICE, 0, &[source, 77]),
            ]
        );
        assert_eq!(window.board.primary.text.as_deref(), Some("Welcome"));
        assert!(
            window.board.clipboard.text.is_none(),
            "the clipboard is apart"
        );
        // The primary selection's send carries a right td-term writes to.
        let (mut reader, writer) = std::io::pipe().unwrap();
        peer::push_descriptor(window.client.connection(), writer.into()).unwrap();
        window.event(text(source, 0, td_ui::data::UTF8)).unwrap();
        assert!(window.board.primary.outgoing.is_some());
        window.transfers(window.clock, true).unwrap();
        assert!(window.board.primary.outgoing.is_none());
        let mut sent = String::new();
        reader.read_to_string(&mut sent).unwrap();
        assert_eq!(sent, "Welcome");
        // Output clears what is shown; what was offered stays offered.
        window.output(b"!").unwrap();
        assert!(window.selection.is_none());
        assert_eq!(window.client.primary_source(), Some(source));
        // The compositor's cancel drops the text.
        window.event(message(source, 1, &[])).unwrap();
        assert!(window.board.primary.text.is_none());
    }

    /// A drag released past the surface's edge comes as release, leave,
    /// frame; the frame still finishes it and offers it. A drag the
    /// pointer leaves while held is abandoned.
    #[test]
    fn a_drag_released_outside_the_surface_still_becomes_primary() {
        let (mut window, peer) = presented();
        window.output(b"Welcome to td").unwrap();
        press_at(&mut window, 0, 1000, 2, true);
        point(&mut window, 6);
        window.event(message(POINTER, 5, &[])).unwrap();
        peer::drain(&peer).unwrap();
        window
            .event(message(POINTER, 3, &[3, 0, LEFT_BUTTON, 0]))
            .unwrap();
        window.event(message(POINTER, 1, &[4, SURFACE])).unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        assert_eq!(window.selected_text().unwrap().as_deref(), Some("Welcome"));
        let source = window.client.primary_source().unwrap();
        let (requests, _) = peer::drain(&peer).unwrap();
        assert!(requests.contains(&message(PRIMARY_DEVICE, 0, &[source, 3])));
        assert_eq!(window.drag.anchor, None);
        // Held when the pointer leaves: nothing is finished or offered.
        window
            .event(message(POINTER, 0, &[5, SURFACE, 128, 128]))
            .unwrap();
        press_at(&mut window, 0, 3000, 6, true);
        point(&mut window, 2);
        window.event(message(POINTER, 5, &[])).unwrap();
        peer::drain(&peer).unwrap();
        window.event(message(POINTER, 1, &[7, SURFACE])).unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        assert_eq!(window.drag.anchor, None);
        assert_eq!(window.client.primary_source(), Some(source));
        assert!(peer::drain(&peer).unwrap().0.is_empty());
    }

    /// A middle press pastes the primary selection, read from its offer;
    /// with nothing to paste it does nothing, bell included. A change of
    /// the primary selection cancels its paste, the clipboard's does not.
    #[test]
    fn a_middle_press_pastes_the_primary_selection() {
        let (mut window, peer) = presented();
        focus(&mut window, 5);
        window.stale = false;
        let middle = |window: &mut Window| {
            window
                .event(message(POINTER, 3, &[9, 0, MIDDLE_BUTTON, 1]))
                .unwrap();
            window
                .event(message(POINTER, 3, &[10, 0, MIDDLE_BUTTON, 0]))
                .unwrap();
            window.event(message(POINTER, 5, &[])).unwrap();
        };
        middle(&mut window);
        assert!(window.board.incoming.is_none());
        assert!(!window.stale, "no bell");
        let offer = 0xff00_0002;
        window.event(message(PRIMARY_DEVICE, 0, &[offer])).unwrap();
        window.event(text(offer, 0, "text/plain")).unwrap();
        window.event(message(PRIMARY_DEVICE, 1, &[offer])).unwrap();
        peer::drain(&peer).unwrap();
        middle(&mut window);
        assert!(matches!(
            window.board.incoming,
            Some((_, td_ui::data::Board::Primary))
        ));
        let (requests, files) = peer::drain(&peer).unwrap();
        assert_eq!(requests, [text(offer, 0, "text/plain")]);
        // The clipboard's selection changing leaves it.
        window.event(message(DEVICE, 5, &[0])).unwrap();
        assert!(window.board.incoming.is_some());
        let mut file = files.into_iter().next().unwrap();
        file.write_all(b"echo hi").unwrap();
        drop(file);
        window.transfers(window.clock, true).unwrap();
        assert!(window.board.incoming.is_none());
        assert_eq!(window.input.take_for_test(), b"echo hi");
        // A copy changes only the clipboard, so it leaves it too.
        middle(&mut window);
        assert!(window.board.incoming.is_some());
        window.output(b"copied").unwrap();
        window.selection = selection((0, 0), (0, 5));
        modifiers(&mut window, SHIFT | CONTROL);
        press(&mut window, 46);
        assert_eq!(window.board.clipboard.text.as_deref(), Some("copied"));
        assert!(window.board.incoming.is_some());
        // Its own selection changing cancels it.
        window.event(message(PRIMARY_DEVICE, 1, &[0])).unwrap();
        assert!(window.board.incoming.is_none());
    }

    #[test]
    fn the_copy_bound_is_on_the_trimmed_text() {
        let (mut window, _peer) = presented();
        // The largest grid a 32 MiB frame holds, 128 rows of 512. Full rows
        // up to the last, which is one character and 511 blanks: the text
        // copied is under the bound, the row untrimmed would carry it over.
        window
            .adopt(Size {
                width: 4096,
                height: 2048,
            })
            .unwrap();
        assert_eq!(window.cells, Some((128, 512)));
        let full = "x".repeat(512);
        let mut rows = vec![full.as_str(); 127];
        rows.push("x");
        window.output(rows.join("\r\n").as_bytes()).unwrap();
        window.selection = Some(render::Selection {
            anchor: (0, 0),
            extent: (127, 511),
        });
        let text = window.selected_text().unwrap().unwrap();
        assert!(text.len() <= MAX_CLIPBOARD_BYTES);
        assert!(text.len() + 511 > MAX_CLIPBOARD_BYTES);
        assert_eq!(text, rows.join("\n"));
    }

    #[test]
    fn output_during_a_drag_does_not_strand_the_rest_of_it() {
        let (mut window, _peer) = presented();
        window.output(b"Welcome to td").unwrap();
        let fixed = |cells: u32, size: u32| cells * size * 256 + 128;
        window
            .event(message(
                POINTER,
                0,
                &[1, SURFACE, fixed(0, 8), fixed(0, 16)],
            ))
            .unwrap();
        window
            .event(message(POINTER, 3, &[2, 0, LEFT_BUTTON, 1]))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        assert!(window.selection.is_none(), "a press alone selects nothing");
        window
            .event(message(POINTER, 2, &[0, fixed(3, 8), fixed(0, 16)]))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        assert!(window.selection.is_some());
        window.output(b"!").unwrap();
        assert!(window.selection.is_none(), "output clears what it moved");
        window
            .event(message(POINTER, 2, &[0, fixed(6, 8), fixed(0, 16)]))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        window
            .event(message(POINTER, 3, &[3, 0, LEFT_BUTTON, 0]))
            .unwrap();
        window.event(message(POINTER, 5, &[])).unwrap();
        assert_eq!(window.selected_text().unwrap().as_deref(), Some("Welcome"));
        assert_eq!(window.drag.anchor, None, "the release ends the drag");
    }

    #[test]
    fn the_proof_waits_for_the_server_sync_of_the_live_source() {
        let (mut window, peer) = presented();
        window.proof.enabled = true;
        focus(&mut window, 5);
        window.output(b"Welcome").unwrap();
        window.selection = Some(render::Selection {
            anchor: (0, 0),
            extent: (0, 6),
        });
        peer::drain(&peer).unwrap();
        modifiers(&mut window, SHIFT | CONTROL);
        press(&mut window, 46);
        let (requests, _) = peer::drain(&peer).unwrap();
        let sync = requests
            .iter()
            .find(|m| m.object == DISPLAY && m.opcode == 0)
            .map(|m| Cursor::new(&m.payload).u32().unwrap())
            .unwrap();
        assert_eq!(window.client.kind(sync).unwrap(), Kind::App(Object::Sync));
        window.event(message(sync, 0, &[0])).unwrap();
        assert!(window.board.sync.is_none());
        window.event(message(DISPLAY, 1, &[sync])).unwrap();
        assert_eq!(window.client.kind(sync).unwrap(), Kind::Free);
    }

    #[test]
    fn a_paste_with_nothing_offered_rings_without_reaching_the_child() {
        let (mut window, _peer) = presented();
        focus(&mut window, 5);
        window.stale = false;
        modifiers(&mut window, SHIFT | CONTROL);
        press(&mut window, 47);
        assert!(window.input.take_for_test().is_empty());
        assert!(window.board.incoming.is_none());
        assert!(window.stale, "the bell is drawn");
    }

    #[test]
    fn a_selected_offer_is_received_over_a_fresh_endpoint() {
        let (mut window, peer) = presented();
        focus(&mut window, 5);
        let offer = 0xff00_0001;
        window.event(message(DEVICE, 0, &[offer])).unwrap();
        window.event(text(offer, 0, "text/plain")).unwrap();
        window.event(message(DEVICE, 5, &[offer])).unwrap();
        peer::drain(&peer).unwrap();
        modifiers(&mut window, SHIFT | CONTROL);
        press(&mut window, 47);
        assert!(window.board.incoming.is_some());
        let (requests, files) = peer::drain(&peer).unwrap();
        assert!(requests.iter().any(|m| m.object == offer && m.opcode == 1));
        assert_eq!(files.len(), 1);
        // A new selection cancels the transfer in progress.
        window.event(message(DEVICE, 5, &[0])).unwrap();
        assert!(window.board.incoming.is_none());
    }

    #[test]
    fn output_feeds_the_model_and_its_replies_go_to_the_child() {
        let (mut window, _peer) = presented();
        window.stale = false;
        window.output(b"hi\x1b[6n").unwrap();
        assert!(window.stale);
        assert_eq!(window.input.take_for_test(), b"\x1b[1;3R");
        let sender = window.sender.clone().unwrap();
        sender.send(Event::Output(b"!".to_vec())).unwrap();
        sender
            .send(Event::Closed("the reader failed".into()))
            .unwrap();
        assert_eq!(window.drain().unwrap_err(), "the reader failed");
        let terminal = window.model.as_ref().unwrap();
        assert_eq!(terminal.cell(0, 2).map(|cell| cell.scalar), Some('!'));
    }

    #[test]
    fn a_frame_is_bounded_before_anything_is_allocated_for_it() {
        assert!(frame_bytes(Size {
            width: 0,
            height: 10
        })
        .is_err());
        assert!(frame_bytes(Size {
            width: MAX_AXIS + 1,
            height: 1
        })
        .is_err());
        assert_eq!(
            frame_bytes(Size {
                width: 640,
                height: 320
            })
            .unwrap(),
            640 * 320 * 4
        );
    }

    #[test]
    fn the_default_surface_holds_exactly_the_default_grid() {
        selftest().unwrap();
        let font = td_ui::font::pinned().unwrap();
        assert_eq!(
            grid(
                Size {
                    width: 3,
                    height: 3
                },
                render::cell_size(&font, None)
            )
            .unwrap(),
            (1, 1),
            "a surface smaller than a cell still names a grid"
        );
    }

    #[test]
    fn a_paste_is_text_only_and_bracketed_when_asked() {
        assert_eq!(paste_input(b"ls\n".to_vec(), false).unwrap(), b"ls\n");
        assert_eq!(
            paste_input(b"ls".to_vec(), true).unwrap(),
            b"\x1b[200~ls\x1b[201~"
        );
        assert!(paste_input(b"\x1b[201~rm".to_vec(), true).is_err());
        assert!(paste_input(vec![0xff], false).is_err());
        assert!(paste_input(Vec::new(), true).unwrap().is_empty());
        assert!(paste_input(vec![b'x'; keys::MAX_INPUT_BYTES - 11], true).is_err());
        assert_eq!(
            paste_input(vec![b'x'; keys::MAX_INPUT_BYTES - 12], true)
                .unwrap()
                .len(),
            keys::MAX_INPUT_BYTES
        );
    }

    #[test]
    fn the_proof_authority_is_one_exact_word() {
        assert!(cmdline_has_clipboard_proof(b"quiet td.firefox-input=1 x"));
        assert!(!cmdline_has_clipboard_proof(b"td.firefox-input=10"));
        assert!(!cmdline_has_clipboard_proof(b"xtd.firefox-input=1"));
        assert!(!clipboard_proof_enabled(Path::new("/nonexistent/cmdline")).unwrap());
        assert_eq!(
            clipboard_sent_marker(true, "Welcome").as_deref(),
            Some("TD-TERM-CLIPBOARD-SENT bytes=7\n")
        );
        assert!(clipboard_sent_marker(false, "Welcome").is_none());
        assert!(clipboard_sent_marker(true, "Welcome!").is_none());
    }

    #[test]
    fn a_launched_program_is_named_by_its_bounded_final_component() {
        assert_eq!(launched_program_name(&[]), None);
        assert_eq!(
            launched_program_name(&["/bin/claude".into(), "--version".into()]).as_deref(),
            Some("claude")
        );
        let long = format!("/bin/{}", "x".repeat(40));
        assert_eq!(launched_program_name(&[long.into()]).unwrap().len(), 32);
        assert_eq!(
            launched_program_name(&["/bin/a\u{2028}b".into()]).as_deref(),
            Some("a b")
        );
    }

    #[test]
    fn a_launched_child_that_ends_badly_reports_its_last_screen() {
        use std::os::unix::process::ExitStatusExt;
        let mut terminal = Terminal::new(4, 20).unwrap();
        terminal.feed("error: no\r\n\r\nbye\u{202e}".as_bytes());
        let failed = ExitStatus::from_raw(1 << 8);
        let report = last_screen_report(Some("claude"), failed, Some(&terminal)).unwrap();
        assert_eq!(
            report,
            "\ntd-term: last screen (claude): error: no\n\
             td-term: last screen (claude): \n\
             td-term: last screen (claude): bye\n"
        );
        assert!(last_screen_report(None, failed, Some(&terminal)).is_none());
        assert!(last_screen_report(Some("x"), ExitStatus::from_raw(0), Some(&terminal)).is_none());
        assert_eq!(ended(failed), "the terminal's child exited with status 1");
        assert!(ended(ExitStatus::from_raw(9)).starts_with("the terminal's child was killed"));
    }

    #[test]
    fn the_clipboard_target_is_the_proof_word_on_screen() {
        let mut terminal = Terminal::new(3, 20).unwrap();
        assert_eq!(clipboard_target(&terminal), None);
        terminal.feed(b"\r\n  Welcome");
        assert_eq!(clipboard_target(&terminal), Some((1, 2)));
    }
}
