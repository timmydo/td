//! td-term's Wayland client: the handshake, the frame, and the grid it earns.
//!
//! It presents, and readiness follows the frame as §12 requires — both the
//! buffer release and the frame callback. TWO frames, because a compositor
//! cannot tile a surface it has not mapped: the first configure is zero in
//! both axes, presenting at the fallback is what maps the surface, and the
//! tile arrives in the configure after that. No child yet, so the model
//! rendered here is empty.

use crate::conn::{
    self, Connection, Globals, COMPOSITOR, REGISTRY, SHM, SURFACE, XDG_SURFACE, XDG_TOPLEVEL,
    XDG_WM_BASE,
};
use crate::font::Font;
use crate::pty::Pty;
use crate::scene::SHM_XRGB8888;
use crate::term::Terminal;
use crate::{font, pty, ready, render, socket, wire, MAX_UI_DIMENSION, MAX_UI_FRAME_BYTES};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::mpsc::{sync_channel, SyncSender};
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

/// One past the last fixed id the TERMINAL creates. It binds no seat and
/// creates no keyboard or pointer, so its dynamic range starts three lower
/// than the demo's — and starting where the demo does would SKIP those three,
/// which Wayland forbids. See `conn`'s note.
const FIRST_DYNAMIC_ID: u32 = XDG_TOPLEVEL + 1;

/// What an operator sees in a title bar. td's own compositor parses and
/// discards it; it is set because a client that names itself is easier to
/// identify in a trace than one that does not.
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
}

impl Bound {
    fn interface(self, name: u32) -> Option<&'static str> {
        match name {
            _ if name == self.compositor => Some("wl_compositor"),
            _ if name == self.shm => Some("wl_shm"),
            _ if name == self.xdg_wm_base => Some("xdg_wm_base"),
            _ => None,
        }
    }
}

/// Bind the three globals a terminal needs. No `wl_seat`: there is no
/// keyboard to route yet, and binding one would mean fielding capability
/// events for a device nothing reads.
fn bind_globals(connection: &mut Connection) -> Result<Bound, String> {
    let globals = conn::discover_globals(connection)?;
    let (compositor_name, compositor_version) =
        Globals::require(globals.compositor(), "wl_compositor", 4, 4)?;
    let (shm_name, shm_version) = Globals::require(globals.shm(), "wl_shm", 1, 1)?;
    let (xdg_name, xdg_version) = Globals::require(globals.xdg_wm_base(), "xdg_wm_base", 1, 1)?;
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
    Ok(Bound {
        compositor: compositor_name,
        shm: shm_name,
        xdg_wm_base: xdg_name,
    })
}

/// `XDG_TOPLEVEL_STATE_ACTIVATED`. Pinned by value, as every other protocol
/// number here is: this one decides whether the cursor is drawn as holding
/// the keyboard, so a wrong constant is a terminal whose cursor never claims
/// focus — with nothing failing anywhere.
const XDG_STATE_ACTIVATED: u32 = 4;

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

    /// Readiness, as §12 defines it: a frame the compositor has both released
    /// and presented, drawn FOR what the surface now holds, at a size the
    /// compositor chose. All three matter and the middle one is easy to lose
    /// — a configure can arrive while a frame is in the air, and the frame
    /// that comes back is then a picture of the wrong thing.
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

fn build_pixels(
    size: Size,
    terminal: &Terminal,
    font: &Font,
    palette: &render::Palette,
    focused: bool,
) -> Result<Vec<u8>, String> {
    let bytes = frame_bytes(size)?;
    let mut pixels = vec![0u8; bytes];
    let snapshot = render::Snapshot::new(terminal, focused, false);
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
    let pixels = build_pixels(size, terminal, font, palette, surface.activated)?;
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

/// What the terminal has learned about its child's ending. BOTH halves are
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
#[derive(Default)]
struct Ending {
    drained: bool,
    status: Option<ExitStatus>,
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
                    // The terminal binds no seat and asks for no keymap, so
                    // any descriptor is an event it has misread. `present`
                    // makes the same check of the handshake; this is the rest
                    // of the session, where an unclaimed one would otherwise
                    // be held open until the process ended.
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
    ending: &mut Ending,
    event: Event,
) -> Result<(), String> {
    match event {
        Event::Closed(error) => return Err(error),
        Event::Exit(status) => ending.status = Some(status),
        Event::Drained => ending.drained = true,
        Event::Wayland(message) => {
            if !connection.handle_common(&message)? {
                surface.dispatch(connection, &message, fallback)?;
            }
        }
        Event::Output(bytes) => {
            let terminal = model
                .as_mut()
                .ok_or_else(|| "the child wrote before the terminal had a model".to_string())?;
            terminal.feed(&bytes);
            // Drained and DISCARDED. A reply is an answer the child asked
            // for, and answering needs the writer thread §12 puts beside this
            // one, which lands with the keyboard; leaving them here instead
            // would grow a buffer for every query a program makes.
            terminal.take_replies();
            surface.stale = true;
        }
    }
    // Both halves, in either order.
    if let (true, Some(status)) = (ending.drained, ending.status) {
        return Err(ended(status));
    }
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
) -> Result<Vec<JoinHandle<Result<(), String>>>, String> {
    let output = pty
        .master()
        .try_clone()
        .map_err(|e| format!("duplicate the terminal device for its reader: {e}"))?;
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
    Ok(vec![waiter, reader])
}

/// What a started terminal leaves behind it: the child's two threads, and
/// the readiness socket for as long as the process is up.
type Running = (Vec<JoinHandle<Result<(), String>>>, socket::Published);

/// Start the child, and only then advertise the terminal.
///
/// The ORDER is the contract: resolving the account, spawning the shell and
/// spawning its two threads can each fail, and a probe told the terminal is up
/// on a terminal whose shell never started has been told something that was
/// never true. It is a function rather than two lines of `run` because `run`
/// dials a socket and never returns, so an ordering asserted only there is one
/// no test can watch.
fn start(
    pty: &Pty,
    events: &SyncSender<Event>,
    status: &Path,
    passwd: &Path,
    ready_socket: &Path,
    cells: (u16, u16),
) -> Result<Running, String> {
    let account = pty::current_account(status, passwd)?;
    let command = pty::child_command(Path::new(pty::CTTYHACK), &[])?;
    let children = start_child(pty, events, &command, &account)?;
    let (rows, columns) = cells;
    let published = ready::publish(ready_socket, rows, columns)?;
    Ok((children, published))
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
) -> Result<(), String> {
    let message = connection.next()?;
    // Startup has no child, so nothing here can reach an ending: the two
    // events that make one are produced by threads `start` has not spawned.
    serve_event(
        connection,
        surface,
        model,
        session,
        fallback,
        &mut Ending::default(),
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
) -> Result<(Surface, Terminal, (u16, u16)), String> {
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
    while !surface.ready() {
        serve_turn(connection, &mut surface, &mut model, &session, fallback)?;
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
    Ok((surface, terminal, cells))
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
    let (surface, terminal, cells) = present(connection, directory, font, palette, &pty)?;
    Ok(Prepared {
        surface,
        pty,
        terminal,
        cells,
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
    let (_children, _ready) = start(
        &pty,
        &sender,
        Path::new(PROC_STATUS),
        Path::new(ETC_PASSWD),
        &options.ready_socket,
        (rows, columns),
    )?;
    // `write_all` rather than `print!`, which PANICS on a write failure — and
    // a panic here would abort past `Published::drop` and leave the socket
    // behind. One encoder, so the diagnostic an operator reads and the line a
    // probe parses cannot describe different grids.
    let mut out = std::io::stdout();
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
    let mut ending = Ending::default();
    loop {
        let event = events
            .recv()
            .map_err(|_| "every terminal producer stopped without reporting".to_string())?;
        serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut ending,
            event,
        )?;
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
    /// skipped, and the demo's higher start would skip the three it does not
    /// create.
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
        ];
        used.sort_unstable();
        used.dedup();
        assert_eq!(
            used,
            (1..FIRST_DYNAMIC_ID).collect::<Vec<u32>>(),
            "the terminal's fixed ids are not dense up to its dynamic range"
        );
        // The three it deliberately does not create are the gap the demo's
        // start would leave.
        for absent in [SEAT, KEYBOARD, POINTER] {
            assert!(absent >= FIRST_DYNAMIC_ID);
        }
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
            &mut Ending::default(),
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
            &mut Ending::default(),
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
            &mut Ending::default(),
            Event::Output(b"more".to_vec()),
        )
        .unwrap();
        assert!(!surface.stale);
        assert!(surface.frame_in_flight(), "the redraw produced no frame");
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
            &mut Ending::default(),
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
        )
        .err()
        .unwrap();
        assert!(error.contains("td-term-no-such-passwd"), "{error}");
        assert!(
            !ready_socket.exists(),
            "readiness was published for a terminal with no child"
        );
    }

    /// What the loop does with the two things a child can ask for that are not
    /// pixels. A reply is DROPPED, because answering needs the writer thread
    /// that lands with the keyboard and an answer sent minutes late is worse
    /// than none. The BELL is deliberately kept: §10 says the next submitted
    /// frame inverts a one-pixel ring and clears the bit after release, and
    /// taking it here would destroy the only record that a notification ever
    /// happened.
    #[test]
    fn a_query_is_answered_by_nobody_and_a_bell_waits_to_be_drawn() {
        let (mut connection, _peer) = pair();
        let mut surface = Surface::new(Bound {
            compositor: 1,
            shm: 2,
            xdg_wm_base: 4,
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

        // A cursor-position report and a BEL, which is what a shell prompt
        // and a mistyped completion produce between them.
        serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut Ending::default(),
            Event::Output(b"\x1b[6n\x07".to_vec()),
        )
        .unwrap();
        let terminal = model.as_mut().unwrap();
        assert!(
            terminal.take_replies().is_empty(),
            "a reply was left queued for a writer that does not exist yet"
        );
        assert!(
            terminal.take_bell(),
            "the bell was consumed before anything could present it"
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
        let mut command = fixture_command();
        command.arguments = vec![
            "--exact".into(),
            "term_client::tests::term_client_abort_fixture".into(),
            "--ignored".into(),
        ];
        start_child(&pty, &sender, &command, &account).unwrap();

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

    /// A child that is this test binary, running the ignored fixture below.
    /// `current_exe` rather than `/bin/sh` for `pty.rs`'s reason: the gate's
    /// shell is not td's, and a test that depended on one would be testing the
    /// host.
    fn fixture_command() -> pty::ChildCommand {
        pty::ChildCommand {
            program: std::env::current_exe().unwrap_or_default(),
            arguments: vec![
                "--exact".into(),
                "term_client::tests::term_client_child_fixture".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
        }
    }

    const CHILD_MARKER: &str = "TD-TERM-CHILD-SPOKE";

    /// The child half of the tests below. `#[ignore]` is what keeps it out of
    /// an ordinary run: only a parent naming it exactly, with `--ignored`,
    /// ever reaches it. It cannot be gated on the environment as `pty.rs`'s
    /// fixture is, because the environment its parent gives it is the one
    /// `start_child` constructs — which is the production one, and the point.
    #[test]
    #[ignore = "spawned as the child of a_child_puts_its_own_output_on_the_screen"]
    fn term_client_child_fixture() {
        print!("{CHILD_MARKER}");
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
        start_child(&pty, &sender, &fixture_command(), &account).unwrap();
        drop(sender);

        let mut ending = Ending::default();
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
                &mut ending,
                event,
            );
            if let Err(error) = served {
                ended = Some(error);
            }
        }
        let text = model.as_ref().map(screen).unwrap_or_default();
        assert!(
            text.contains(CHILD_MARKER),
            "the child's output never reached the model: {text:?} (ended: {ended:?})"
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
        start_child(&pty, &sender, &fixture_command(), &account).unwrap();
        let status = loop {
            match events.recv_timeout(Duration::from_secs(30)).unwrap() {
                Event::Exit(status) => break status,
                Event::Output(_) | Event::Drained => {}
                other => panic!("the child produced {}", named(&other)),
            }
        };
        let mut ending = Ending::default();
        // The exit ALONE does not end the terminal: the child's last bytes may
        // still be in the kernel, and `Event::Exit` can overtake them.
        serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut ending,
            Event::Exit(status),
        )
        .unwrap();
        let ended = serve_event(
            &mut connection,
            &mut surface,
            &mut model,
            &session,
            fallback,
            &mut ending,
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
