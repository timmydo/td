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
//! published and the readiness line printed, in that order.
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
use td_ui::face::Face;
use td_ui::font::Font;
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

pub struct Options {
    /// The compositor's socket, or `None` for the environment's endpoint.
    pub socket: Option<PathBuf>,
    pub ready_socket: PathBuf,
    pub working_directory: Option<PathBuf>,
    /// The child's literal argv, or empty for the default shell. See
    /// `session::child_command` for what each means. Bytes rather than text:
    /// a filename argument is whatever the filesystem holds.
    pub command: Vec<OsString>,
}

/// What an operator sees in a title bar; td's compositor keeps it.
pub const TITLE: &str = "td terminal";
const APP_ID: &str = "td-term";

/// Bound on reaching readiness. Past this the compositor is not coming, and
/// a terminal that waits forever is one td-svc reports as down without ever
/// saying why; it is set below the supervisor's 30.
const HANDSHAKE_MS: u64 = 20_000;

/// Where the session's own identity is read from: the uid from the first,
/// everything else from the second.
const PROC_STATUS: &str = "/proc/self/status";
const ETC_PASSWD: &str = "/etc/passwd";
const PROC_CMDLINE: &str = "/proc/cmdline";
const MAX_CMDLINE_BYTES: usize = 4096;
const CLIPBOARD_PROOF_CMDLINE_TOKEN: &[u8] = b"td.firefox-input=1";

const LEFT_BUTTON: u32 = 0x110;
/// The two chords the terminal keeps for itself rather than sending.
const COPY_CHORD: &str = "C-S-c";
const PASTE_CHORD: &str = "C-S-v";
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

/// The pixel size of the fallback grid.
pub fn default_size(font: &Font) -> Result<Size> {
    let width = DEFAULT_COLUMNS
        .checked_mul(font.width())
        .ok_or("the default column count overflows a pixel width")?;
    let height = DEFAULT_ROWS
        .checked_mul(font.height())
        .ok_or("the default row count overflows a pixel height")?;
    Ok(Size { width, height })
}

/// The cell grid a surface of this size holds: `grid_for_tile` is the
/// division the renderer clips to, and `grid_size` the validity the winsize
/// ioctl is held to, so a grid this refuses nothing downstream would take.
pub fn grid(size: Size, font: &Font) -> Result<(u16, u16)> {
    let (rows, columns) = pty::grid_for_tile(size.width, size.height, font.width(), font.height())?;
    let window = pty::grid_size(rows, columns)?;
    Ok((window.rows, window.columns))
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

/// What a frame was drawn FOR: the size, and the activation that decides
/// how the cursor is drawn, so a configure that only takes focus away
/// still needs a new picture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Drawn {
    size: Size,
    activated: bool,
}

/// The frame in flight: complete once the compositor released its buffer
/// AND its frame callback fired, which mean different things.
struct Frame {
    buffer: u32,
    presented: bool,
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
    _ready: ready::Published,
}

/// The clipboard's transfers and the text behind the live source.
#[derive(Default)]
struct Board {
    text: Option<Arc<str>>,
    outgoing: Option<(Outgoing, Arc<str>)>,
    incoming: Option<Incoming>,
    /// The proof's pending sync: its id, the source it confirms, and the
    /// length it will report.
    sync: Option<(u32, u32, usize)>,
}

/// The pointer selection being made: the anchor and extent a frame closes.
#[derive(Default)]
struct Drag {
    position: (i32, i32),
    left_down: bool,
    anchor: Option<(i32, i32)>,
    extent: Option<(i32, i32)>,
    /// The link under a Control-press, read from the screen as it stood at
    /// the press, which the frame closing the press follows.
    link: Option<String>,
    /// A followed press is still held: its drag and release select nothing.
    following: bool,
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
    font: Font,
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
    wheel: Wheel,
    board: Board,
    proof: Proof,
    /// The outline face the cells are drawn in, fitted to the bitmap face's
    /// cell; `None` draws every cell from the bitmap face.
    outline: Option<Face>,
    /// The browser command a link opens with; none, as in production, is
    /// `BROWSER` then `xdg-open` (td-ui's opener).
    browser: Option<String>,
}

impl Window {
    fn new(
        stream: std::os::unix::net::UnixStream,
        options: Options,
        display: String,
    ) -> Result<Self> {
        let font = td_ui::font::pinned()?;
        let fallback = default_size(&font)?;
        let mut client = Client::new(stream, std::env::temp_dir())?;
        let waker = client.connection().waker()?;
        // Before the first frame: a machine whose devpts is missing should
        // fail without having drawn a window.
        let pty = Pty::open()?;
        let (sender, events) = sync_channel(MAX_PENDING_EVENTS);
        Ok(Self {
            client,
            font,
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
            cells: None,
            adopted: None,
            backlog: false,
            stale: false,
            keymap_ready: false,
            viewport: keys::Viewport::new(),
            selection: None,
            drag: Drag::default(),
            wheel: Wheel::default(),
            board: Board::default(),
            proof: Proof::default(),
            outline: None,
            browser: None,
        })
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
        if chord == COPY_CHORD {
            return self.copy(serial);
        }
        if chord == PASTE_CHORD {
            return self.paste();
        }
        self.clear_selection();
        if self.route(chord, stroke.text)? && stroke.repeat && !self.proof.enabled {
            self.client.arm(key, self.clock);
        }
        Ok(())
    }

    fn pointer(&mut self, event: pointer::Event) -> Result<()> {
        match event {
            pointer::Event::Enter { x, y, .. } => self.drag.position = (x, y),
            pointer::Event::Motion(x, y) => {
                self.drag.position = (x, y);
                if self.drag.left_down {
                    self.drag.extent = Some((x, y));
                }
            }
            pointer::Event::Leave(_) => {
                self.drag.left_down = false;
                self.drag.anchor = None;
                self.drag.extent = None;
            }
            pointer::Event::Button {
                button: LEFT_BUTTON,
                pressed,
                ..
            } => {
                self.drag.left_down = pressed;
                if pressed {
                    let held = self.client.input().held();
                    self.drag.following = false;
                    self.drag.link = (held.control && !held.alt && !held.shift)
                        .then(|| self.shown_link(self.drag.position))
                        .flatten();
                    self.drag.anchor = Some(self.drag.position);
                } else if std::mem::take(&mut self.drag.following) {
                    return Ok(());
                }
                self.drag.extent = Some(self.drag.position);
            }
            pointer::Event::Button { .. } => {}
            // A frame is the transaction: the selection and the wheel are
            // applied once when it closes.
            pointer::Event::Frame => {
                if let Some(link) = self.drag.link.take() {
                    self.drag.anchor = None;
                    self.drag.extent = None;
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
                }
                let (rows, _) = self.wheel.frame();
                if rows != 0 {
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
    /// resize's reflow sets), or a committed frame whose callback has not
    /// said it reached the screen, is not what was on screen, and the press
    /// is then a plain one.
    fn shown_link(&self, fixed: (i32, i32)) -> Option<String> {
        let shown = !self.stale && self.frame.as_ref().is_some_and(|frame| frame.presented);
        shown.then(|| self.link_at(fixed)).flatten()
    }

    /// The link under a pointer position in the viewport's row, as td-ui's
    /// rule finds it; none past the drawn grid, whose edge cells a
    /// selection's clamp would reach. A cell holds one scalar, so the row's
    /// text is its cells in order; a link the terminal wrapped onto the next
    /// row is found only up to the row's end.
    fn link_at(&self, fixed: (i32, i32)) -> Option<String> {
        let (rows, columns) = self.cells?;
        let inside = |value: i32, cells: u16, cell: usize| {
            usize::try_from(value.div_euclid(256))
                .is_ok_and(|pixel| pixel < usize::from(cells).saturating_mul(cell))
        };
        if !inside(fixed.0, columns, self.font.width())
            || !inside(fixed.1, rows, self.font.height())
        {
            return None;
        }
        let (row, column) = self.cell_at(fixed)?;
        let terminal = self.model.as_ref()?;
        let viewport = self.viewport.offset(terminal.scrollback());
        let snapshot = render::Snapshot::new(terminal, false, false).scrolled_back(viewport);
        let mut text = String::new();
        let mut at = None;
        // Only the drawn columns: a model wider than the surface is clipped.
        for cell in 0..snapshot.columns().min(usize::from(columns)) {
            if cell == column {
                at = Some(text.len());
            }
            text.push(snapshot.cell(row, cell).scalar);
        }
        let range = td_ui::links::at(&text, at?)?;
        text.get(range).map(str::to_owned)
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
        let (width, height) = (self.font.width(), self.font.height());
        if rows == 0 || columns == 0 || width == 0 || height == 0 {
            return None;
        }
        let pixel = |value: i32| usize::try_from(value.div_euclid(256)).unwrap_or(0);
        let row = (pixel(fixed.1) / height).min(usize::from(rows).saturating_sub(1));
        let column = (pixel(fixed.0) / width).min(usize::from(columns).saturating_sub(1));
        Some((row, column))
    }

    fn select(&mut self, anchor: Option<(i32, i32)>, extent: (i32, i32)) {
        let Some(extent) = self.cell_at(extent) else {
            return;
        };
        let next = match anchor.and_then(|anchor| self.cell_at(anchor)) {
            Some(anchor) => Some(render::Selection { anchor, extent }),
            None => self.selection.map(|selection| render::Selection {
                anchor: selection.anchor,
                extent,
            }),
        };
        if next != self.selection {
            self.selection = next;
            self.stale = true;
        }
    }

    /// The selection's text, rows joined by newlines with each row's
    /// trailing blanks dropped, bounded by the clipboard's ceiling.
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
            if row != start.0 {
                selected.push('\n');
            }
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
            // Bounded after the trim: the ceiling is on what is copied, and
            // one row past it is all the untrimmed text can overshoot by.
            while selected.len() > line_start && selected.ends_with(' ') {
                selected.pop();
            }
            if selected.len() > MAX_CLIPBOARD_BYTES {
                return Err(format!(
                    "terminal selection exceeds {MAX_CLIPBOARD_BYTES} bytes"
                ));
            }
        }
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
        self.board.text = Some(Arc::from(text));
        if self.board.incoming.take().is_some() {
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

    fn paste(&mut self) -> Result<()> {
        if !self.client.clipboard()
            || !self.client.input().focused
            || self.board.incoming.is_some()
            || self.client.selection_mime().is_none()
        {
            self.ring();
            return Ok(());
        }
        let (incoming, peer) = match Incoming::begin(self.clock) {
            Ok(pair) => pair,
            Err(error) => {
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "td-term: paste refused: clipboard endpoint: {error}"
                );
                self.ring();
                return Ok(());
            }
        };
        self.client.receive(&peer)?;
        drop(peer);
        self.board.incoming = Some(incoming);
        Ok(())
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

    fn clipboard(&mut self, event: ClipboardEvent) {
        match event {
            ClipboardEvent::Selection => {
                self.board.incoming = None;
            }
            ClipboardEvent::Send(right) => {
                // A busy send drops exactly its right.
                if self.board.outgoing.is_some() {
                    return;
                }
                let Some(text) = self.board.text.clone() else {
                    return;
                };
                match Outgoing::begin(right, Arc::clone(&text), self.clock) {
                    Ok(transfer) => self.board.outgoing = Some((transfer, text)),
                    Err(error) => {
                        let _ = writeln!(
                            std::io::stderr().lock(),
                            "td-term: clipboard send refused: {error}"
                        );
                    }
                }
            }
            ClipboardEvent::Cancelled => self.board.text = None,
            ClipboardEvent::Released => {
                self.board.incoming = None;
                if let Some((transfer, _)) = self.board.outgoing.take() {
                    let _ = transfer.cancel();
                }
                self.board.text = None;
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
            .is_some_and(|incoming| due(incoming.expired(now)))
        {
            if let Some(mut incoming) = self.board.incoming.take() {
                match incoming.step(now) {
                    Ok(false) => self.board.incoming = Some(incoming),
                    Ok(true) => match incoming.finish() {
                        Ok(text) if self.client.input().focused => self.pasted(text)?,
                        Ok(_) => {}
                        Err(_) => self.ring(),
                    },
                    Err(_) => self.ring(),
                }
            }
        }
        if self
            .board
            .outgoing
            .as_ref()
            .is_some_and(|(outgoing, _)| due(outgoing.expired(now)))
        {
            if let Some((mut outgoing, text)) = self.board.outgoing.take() {
                match outgoing.step(now) {
                    Ok(false) => self.board.outgoing = Some((outgoing, text)),
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
                            "td-term: clipboard send failed: {error}"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// How long the next turn may wait for an event, in milliseconds, never
    /// zero: until a repeat is due, a transfer steps, the handshake expires,
    /// or at once while the child's output is still queued.
    fn next_wait(&self, now: u64) -> u64 {
        if self.backlog {
            return 1;
        }
        let mut wait = self.client.wait_ms(now);
        if self.board.incoming.is_some() || self.board.outgoing.is_some() {
            wait = wait.min(TRANSFER_WAIT_MS);
        }
        if self.child.is_none() {
            wait = wait.min(HANDSHAKE_MS.saturating_sub(now).max(1));
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
        self.layout_configured
            && self.drawn == self.wanted()
            && self.adopted == self.current
            && self.frame_complete()
    }

    /// Readiness, and a keyboard whose keymap compiled: a shell nobody can
    /// type into is not a terminal.
    fn presented(&self) -> bool {
        self.ready() && self.client.keyboard().is_some() && self.keymap_ready
    }

    /// The size changed: bound it, set the PTY and verify it took, then
    /// reflow the model — the child learns the grid before the pixels move.
    fn adopt(&mut self, size: Size) -> Result<()> {
        frame_bytes(size)?;
        let (rows, columns) = grid(size, &self.font)?;
        self.clear_selection();
        let window = self.pty.resize(usize::from(rows), usize::from(columns))?;
        let (rows, columns) = (usize::from(window.rows), usize::from(window.columns));
        match self.model.as_mut() {
            // Reflowed, not rebuilt: a new model would erase the session.
            Some(terminal) => terminal.resize(rows, columns)?,
            None => self.model = Some(Terminal::new(rows, columns)?),
        }
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
        let account = session::current_account(Path::new(PROC_STATUS), Path::new(ETC_PASSWD))?;
        let command = session::child_command(Path::new(session::CTTYHACK), &self.options.command)?;
        let program = launched_program_name(&self.options.command);
        let directory = self
            .options
            .working_directory
            .clone()
            .unwrap_or_else(|| PathBuf::from(&account.home));
        if !directory.is_absolute() {
            return Err("terminal working directory is not absolute".into());
        }
        let environment = session::environment(
            &account,
            std::env::var("TD_CONTROL_SOCKET").ok().as_deref(),
            &self.wayland_display,
        );
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
        let published = ready::publish(&self.options.ready_socket, rows, columns)?;
        self.child = Some(Child {
            drained: false,
            status: None,
            program,
            _threads: vec![waiter, reader, writer],
            _ready: published,
        });
        // One locked write of one line, so a marker cannot interleave with
        // another thread's output and reach a reader as neither.
        let mut out = std::io::stdout().lock();
        out.write_all(ready::marker(rows, columns).as_bytes())
            .and_then(|()| out.flush())
            .map_err(|e| format!("write terminal ready marker: {e}"))
    }

    /// The child is gone and its output has run out: report, and end.
    fn finished(&self) -> Option<String> {
        let child = self.child.as_ref()?;
        let status = child.status.filter(|_| child.drained)?;
        if let Some(report) =
            last_screen_report(child.program.as_deref(), status, self.model.as_ref())
        {
            let _ = std::io::stderr().lock().write_all(report.as_bytes());
        }
        Some(ended(status))
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
                return Err("compositor requested that the terminal close".into())
            }
            Handled::FrameDone => {
                if let Some(frame) = self.frame.as_mut() {
                    frame.presented = true;
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
            Handled::Clipboard(event) => self.clipboard(event),
            Handled::Capabilities { keyboard, pointer } => {
                if !keyboard {
                    self.keymap_ready = false;
                    self.board.incoming = None;
                }
                if !pointer {
                    self.drag = Drag::default();
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
        self.drain()?;
        if let Some(ended) = self.finished() {
            return Err(ended);
        }
        if idle {
            if let Some(stroke) = self.client.repeat(now)? {
                // Rerouted per repetition: the mode and the view it asks
                // about may have changed since the press.
                if !self.route(&stroke.chord, stroke.text)? {
                    self.client.cancel_repeat();
                }
            }
        }
        self.transfers(now, idle)?;
        if self.child.is_none() {
            if self.presented() {
                self.start()?;
            } else if now > HANDSHAKE_MS {
                return Err(format!(
                    "the terminal was not ready within {} s",
                    HANDSHAKE_MS / 1000
                ));
            }
        }
        self.markers()?;
        let wait = self.next_wait(now);
        self.client
            .connection()
            .set_wait(Duration::from_millis(wait));
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        let Some(wanted) = self.wanted() else {
            return Ok(());
        };
        // Throttled on the frame in flight: only the latest state is drawn,
        // because what comes between is superseded before anything is
        // allocated for it.
        let resize = self.adopted != Some(wanted.size);
        if self.client.can_present() && (self.drawn != Some(wanted) || self.stale || resize) {
            if resize {
                self.adopt(wanted.size)?;
            }
            let terminal = self
                .model
                .as_ref()
                .ok_or("the terminal has no model to draw")?;
            let viewport = self.viewport.offset(terminal.scrollback());
            let snapshot = render::Snapshot::new(terminal, wanted.activated, false)
                .scrolled_back(viewport)
                .with_selection(self.selection);
            let (palette, font, outline) = (&self.palette, &self.font, &mut self.outline);
            let (width, height) = (wanted.size.width, wanted.size.height);
            let presented = self.client.present(width, height, &mut |pixels| {
                let outline = outline.as_mut();
                render::render_with(&snapshot, palette, font, outline, pixels, width, height)
            })?;
            if presented {
                self.drawn = Some(wanted);
                self.stale = false;
                self.needs_commit = false;
                self.frame = self.client.presented().map(|buffer| Frame {
                    buffer,
                    presented: false,
                });
                return Ok(());
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

/// A child ending ends the terminal; the words say which ending it was.
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

pub fn run(options: Options) -> Result<()> {
    let proof = clipboard_proof_enabled(Path::new(PROC_CMDLINE))?;
    let endpoint = match &options.socket {
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
    // The pinned outline face in four styles, fitted to the bitmap cell so
    // the grid and every rule stay Unifont's; without it, Unifont.
    let setting = std::env::var_os(td_ui::pinned_face::SETTING);
    let (width, height) = (window.font.width(), window.font.height());
    window.outline =
        td_ui::pinned_face::styles_or_note("td-term", width, height, setting.as_deref());
    td_ui::client::run(&mut window)
}

pub fn selftest() -> Result<()> {
    let font = td_ui::font::pinned()?;
    let fallback = default_size(&font)?;
    let (rows, columns) = grid(fallback, &font)?;
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
    const POINTER: u32 = 13;
    const KEYBOARD: u32 = 14;
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

    /// A window bound to a scripted compositor offering a seat and a
    /// clipboard, both devices created: `(window, peer)`.
    fn fixture() -> (Window, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        theirs
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let options = Options {
            socket: None,
            ready_socket: std::env::temp_dir().join("td-term-unused-ready"),
            working_directory: None,
            command: Vec::new(),
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
        assert_eq!(window.board.text.as_deref(), Some("Welcome"));
        assert!(window.selection.is_some(), "a copy keeps the selection");
        assert!(window.board.sync.is_none(), "no proof, no sync");
    }

    /// A pointer at a column of the first row.
    fn point(window: &mut Window, column: u32) {
        window
            .event(message(POINTER, 2, &[0, column * 8 * 256 + 128, 128]))
            .unwrap();
    }

    /// A left press, or release, and the frame that closes it.
    fn click(window: &mut Window, pressed: bool) {
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

    fn bell(window: &mut Window) -> bool {
        window.model.as_mut().unwrap().take_bell()
    }

    /// A Control-press over a link opens it through td-ui's opener (here a
    /// browser that cannot start, so the bell rings) and selects nothing:
    /// the selection stays through the press, its drag and its release.
    /// Control off a link, and a press without Control on one, are plain
    /// presses that select their cell.
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
        let cell = |column| {
            Some(render::Selection {
                anchor: (0, column),
                extent: (0, column),
            })
        };
        assert_eq!(window.selection, cell(1));
        // Without Control a press on the link selects its cell.
        modifiers(&mut window, 0);
        shown(&mut window);
        point(&mut window, 10);
        click(&mut window, true);
        click(&mut window, false);
        assert_eq!(window.selection, cell(10));
        assert!(!bell(&mut window));
        // Nor with Shift or Alt beside Control: the press is Control's alone.
        for (other, column) in [(SHIFT, 11), (ALT, 12)] {
            modifiers(&mut window, CONTROL | other);
            shown(&mut window);
            point(&mut window, column);
            click(&mut window, true);
            click(&mut window, false);
            assert_eq!(window.selection, cell(usize::try_from(column).unwrap()));
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
        assert_eq!(window.selection, cell(10));

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
                &font
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
