//! Wayland presentation and input, with an optional asynchronous file session.

use crate::control_jobs::ReloadOutcome;
use crate::dialog::{Close, Closed, Conflict, Scope, Target};
use crate::font::Font;
use crate::keyboard::{Keymap, Modifiers};
use crate::keys::Profile;
use crate::render::{Draw, Geometry, GlyphStyle, Label, Primitive, Raster, Rect, CHROME, INK};
use crate::seat::Input;
use crate::ui::{Controller, Event, Outcome};
use crate::wire::{self, Builder, Cursor, Message};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;
const DISPLAY: u32 = 1;
const REGISTRY: u32 = 2;
const SYNC: u32 = 3;
const COMPOSITOR: u32 = 4;
const SHM: u32 = 5;
const WM: u32 = 6;
const SURFACE: u32 = 7;
const XDG_SURFACE: u32 = 8;
const TOPLEVEL: u32 = 9;
const OBJECTS: usize = 128;
const READ_BYTES: usize = 16 * 1024;
const PENDING_BYTES: usize = 128 * 1024;
const CONTROL_JOBS_PER_TURN: usize = 2;
const INITIAL_DEADLINE: Duration = Duration::from_secs(20);
const WRITE_DEADLINE: Duration = Duration::from_secs(5);
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

fn cursor_pixels() -> [u8; 16 * 24 * 4] {
    let rows = [
        "#",
        "##",
        "#+#",
        "#++#",
        "#+++#",
        "#++++#",
        "#+++++#",
        "#++++++#",
        "#+++++++#",
        "#++++++++#",
        "#+++++++++#",
        "#++++++++++#",
        "#+++++++#####",
        "#++++#++#",
        "#+++# #++#",
        "#++#  #++#",
        "#+#    #++#",
        "##     #++#",
        "#      ####",
        "",
        "",
        "",
        "",
        "",
    ];
    let mut pixels = [0; 16 * 24 * 4];
    for (row, output) in rows.iter().zip(pixels.as_chunks_mut::<64>().0) {
        for (cell, pixel) in row.bytes().zip(output.as_chunks_mut::<4>().0) {
            let value: u32 = match cell {
                b'#' => 0xff48453f,
                b'+' => 0xfff0eadf,
                _ => 0,
            };
            pixel.copy_from_slice(&value.to_le_bytes());
        }
    }
    pixels
}

fn error(value: impl std::fmt::Display) -> String {
    value.to_string()
}

#[derive(Debug, Eq, PartialEq)]
enum Endpoint {
    Path(PathBuf),
    Inherited(i32),
}

fn endpoint(
    socket: Option<OsString>,
    display: Option<OsString>,
    runtime: Option<OsString>,
) -> Result<Endpoint> {
    if let Some(socket) = socket {
        let value = socket
            .to_str()
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .ok_or("invalid WAYLAND_SOCKET")?
            .parse::<i32>()
            .map_err(error)?;
        if value < 3 {
            return Err("WAYLAND_SOCKET must name a descriptor >= 3".into());
        }
        return Ok(Endpoint::Inherited(value));
    }
    let display = PathBuf::from(display.unwrap_or_else(|| "wayland-0".into()));
    if display.as_os_str().is_empty() {
        return Err("empty WAYLAND_DISPLAY".into());
    }
    if display.is_absolute() {
        return Ok(Endpoint::Path(display));
    }
    let runtime =
        PathBuf::from(runtime.ok_or("relative WAYLAND_DISPLAY requires XDG_RUNTIME_DIR")?);
    if !runtime.is_absolute() {
        return Err("XDG_RUNTIME_DIR must be absolute".into());
    }
    Ok(Endpoint::Path(runtime.join(display)))
}

fn connect(endpoint: Endpoint) -> Result<UnixStream> {
    match endpoint {
        Endpoint::Inherited(fd) => crate::sys::inherited(fd).map_err(error),
        Endpoint::Path(path) => {
            // A full Unix listen queue can block connect. One worker owns the
            // attempt; if the deadline wins, any eventual stream is dropped.
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            std::thread::Builder::new()
                .name("editor-connect".into())
                .spawn(move || {
                    let _ = sender.send(UnixStream::connect(path));
                })
                .map_err(error)?;
            receiver
                .recv_timeout(Duration::from_secs(5))
                .map_err(|e| format!("Wayland connect: {e}"))?
                .map_err(error)
        }
    }
}

struct Connection {
    stream: UnixStream,
    pending: Vec<u8>,
    read: [u8; READ_BYTES],
    startup_deadline: Option<Instant>,
    descriptors: VecDeque<OwnedFd>,
    wait: Duration,
}

impl Connection {
    fn new(stream: UnixStream) -> Result<Self> {
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(error)?;
        Ok(Self {
            stream,
            pending: Vec::with_capacity(PENDING_BYTES),
            read: [0; READ_BYTES],
            startup_deadline: None,
            descriptors: VecDeque::with_capacity(8),
            wait: Duration::from_millis(100),
        })
    }

    fn send(&mut self, object: u32, opcode: u16, body: Builder, pool: Option<&File>) -> Result<()> {
        let bytes = body.message(object, opcode)?;
        let deadline = Instant::now() + self.budget(WRITE_DEADLINE)?;
        let mut offset = 0;
        let mut pool = pool;
        while offset < bytes.len() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or("Wayland write deadline")?;
            self.stream
                .set_write_timeout(Some(remaining))
                .map_err(error)?;
            let suffix = bytes.get(offset..).ok_or("Wayland write offset")?;
            let sent = if let Some(file) = pool {
                crate::sys::send_file(&self.stream, suffix, file)
            } else {
                self.stream.write(suffix)
            };
            match sent {
                Ok(0) => return Err("Wayland write returned zero".into()),
                Ok(count) if count <= suffix.len() => {
                    offset += count;
                    pool = None;
                }
                Ok(_) => return Err("Wayland write exceeded its buffer".into()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    std::thread::sleep(
                        Duration::from_millis(5)
                            .min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
                Err(e) => return Err(format!("Wayland write: {e}")),
            }
        }
        Ok(())
    }

    fn words(&mut self, object: u32, opcode: u16, words: &[u32]) -> Result<()> {
        let mut body = Builder::new();
        for word in words {
            body.u32(*word);
        }
        self.send(object, opcode, body, None)
    }

    fn read_more(&mut self) -> Result<()> {
        let wait = self.budget(self.wait)?;
        self.stream.set_read_timeout(Some(wait)).map_err(error)?;
        let start = Instant::now();
        match crate::sys::receive(&self.stream, &mut self.read) {
            Ok((0, _)) => Err("Wayland compositor disconnected".into()),
            Ok((count, fds)) => {
                if self.pending.len().saturating_add(count) > PENDING_BYTES
                    || self.descriptors.len().saturating_add(fds.len()) > 8
                {
                    return Err("Wayland receive budget".into());
                }
                self.descriptors.extend(fds);
                self.pending
                    .extend_from_slice(self.read.get(..count).ok_or("Wayland receive length")?);
                Ok(())
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                // Inherited nonblocking sockets do not honor SO_RCVTIMEO.
                if e.kind() == io::ErrorKind::WouldBlock {
                    std::thread::sleep(wait.saturating_sub(start.elapsed()));
                }
                Ok(())
            }
            Err(e) => Err(format!("Wayland receive: {e}")),
        }
    }

    fn budget(&self, limit: Duration) -> Result<Duration> {
        match self.startup_deadline {
            Some(deadline) => deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .map(|d| d.min(limit))
                .ok_or("Wayland initial commit deadline".into()),
            None => Ok(limit),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Free,
    Fixed,
    Pool,
    Buffer,
    Frame,
    Retired,
    RetiredBuffer,
    Seat,
    Keyboard,
    RetiredKeyboard,
    RetiredSeat,
    Pointer,
    RetiredPointer,
    CursorSurface,
    CursorBuffer,
    ClipboardManager,
    ClipboardDevice,
    RetiredClipboardDevice,
    ClipboardSource,
    RetiredClipboardSource,
    ClipboardSync,
}

struct Buffer {
    id: u32,
    file: File,
    geometry: Geometry,
    busy: bool,
}

struct CursorImage {
    surface: u32,
    buffer: u32,
    busy: bool,
}

#[derive(Default)]
struct Pointer {
    device: Option<u32>,
    enter: Option<u32>,
    x: i32,
    y: i32,
    held: bool,
    wheel: crate::pointer::Wheel,
    wheel_target: Option<Target>,
    wheel_context: Option<Target>,
}

struct Window {
    connection: Connection,
    globals: BTreeMap<u32, (String, u32)>,
    required: Vec<u32>,
    objects: [Kind; OBJECTS],
    ui: Controller,
    font: Font,
    labels: Vec<(crate::model::TabId, &'static str)>,
    buffers: Vec<Buffer>,
    pixels: Vec<u8>,
    configured: bool,
    pending_size: Option<(i32, i32)>,
    xrgb: bool,
    argb: bool,
    pointer: Pointer,
    cursor_image: Option<CursorImage>,
    frames: crate::control_frame::Frames,
    callback: Option<u32>,
    bound: bool,
    closed: bool,
    temporary: PathBuf,
    seat: Option<u32>,
    device: Option<u32>,
    input: Input,
    clock: u64,
    notice: Option<String>,
    quitting: bool,
    files: Option<crate::session::Session>,
    prompt: Option<PathPrompt>,
    closing: Option<Close>,
    last_dialog_id: u64,
    closing_save: bool,
    conflict: Option<Conflict>,
    reloading: Option<Target>,
    menu: Option<crate::menu::Menu>,
    clipboard: crate::data::Clipboard,
    activation_serial: Option<u32>,
    search: Option<crate::search::Prompt>,
    number: Option<crate::number::Prompt>,
    command: Option<crate::command::Prompt>,
    replace: Option<crate::replace::Prompt>,
    searches: crate::search::History,
    spelling: crate::spelling::WindowState,
    control_jobs: crate::control_jobs::Jobs,
    control_file_job: Option<ControlFile>,
    control: Option<crate::control_worker::Worker>,
    frame_waiters: VecDeque<crate::control_worker::Job>,
    control_cleanup_error: Option<String>,
}

enum ControlFile {
    Open(u64),
    Save(u64),
    Reload(u64),
    Dictionary(u64),
}

enum ControlAnswer {
    Done,
    Dialog(u64),
    Job(u64),
}

enum PathAction {
    Open,
    Dictionary,
    Save {
        tab: crate::model::TabId,
        revision: u64,
    },
}
struct PathPrompt {
    action: PathAction,
    text: String,
    identity: Option<PathIdentity>,
}
struct PathIdentity {
    id: u64,
    point: crate::model::RevisionPoint,
}
impl PathPrompt {
    fn notice(&self) -> String {
        let action = match self.action {
            PathAction::Open => "Open",
            PathAction::Dictionary => "Dictionary (read only)",
            PathAction::Save { .. } => "Save As (new path only)",
        };
        // Keep the insertion end visible even for a long path. The full literal
        // value, never this presentation, is passed to the worker.
        let start = self
            .text
            .char_indices()
            .rev()
            .nth(159)
            .map_or(0, |(i, _)| i);
        let tail = self.text.get(start..).unwrap_or_default();
        format!("{action}: literal path, no shell expansion\nReturn: submit; Escape/Ctrl+G: cancel; Ctrl+U: clear\n{}{}|", if tail.len() < self.text.len() { "..." } else { "" }, tail)
    }
}

impl Window {
    fn new(stream: UnixStream, temporary: PathBuf) -> Result<Self> {
        let mut ui = Controller::default();
        let first = match ui.dispatch(Event::Load(b"td-editor scratch preview -- NO SAVE\n\nYou can type, select, undo and switch tabs with the keyboard.\nCtrl+Tab switches tabs. Emacs: M-q fills a paragraph.\n\nMouse selection, tab clicks, scrolling and menus work. F10 opens menus. Clipboard needs data-device v3. Open, Save and spelling are not connected.\nUse --keys=emacs or --keys=windows at startup.\n\nUnicode scalars: caf\xc3\xa9, na\xc3\xafve, \xce\xbb.\n\tTabs advance to eight-column stops.\n\nClosing dirty text asks for explicit discard. Do not use as $EDITOR.\nProcess termination still loses scratch text; keep nothing important here.\n")).map_err(error)? {
            Outcome::Created(tab) => tab, _ => return Err("preview fixture creation".into()),
        };
        let second = match ui
            .dispatch(Event::Load(b"A second tab, for the rendering fixture.\n"))
            .map_err(error)?
        {
            Outcome::Created(tab) => tab,
            _ => return Err("preview fixture creation".into()),
        };
        ui.dispatch(Event::SelectTab(first)).map_err(error)?;
        ui.dispatch(Event::Focus(false)).map_err(error)?;
        let mut objects = [Kind::Free; OBJECTS];
        objects
            .get_mut(..10)
            .ok_or("object table")?
            .fill(Kind::Fixed);
        Ok(Self {
            connection: Connection::new(stream)?,
            globals: BTreeMap::new(),
            required: Vec::new(),
            objects,
            ui,
            font: crate::font::pinned()?,
            labels: vec![(first, "Scratch (no save)"), (second, "Second tab")],
            buffers: Vec::with_capacity(3),
            pixels: Vec::new(),
            configured: false,
            pending_size: None,
            xrgb: false,
            argb: false,
            pointer: Pointer::default(),
            cursor_image: None,
            frames: crate::control_frame::Frames::default(),
            callback: None,
            bound: false,
            closed: false,
            temporary,
            seat: None,
            device: None,
            input: Input::default(),
            clock: 0,
            notice: None,
            quitting: false,
            files: None,
            prompt: None,
            closing: None,
            last_dialog_id: 0,
            closing_save: false,
            conflict: None,
            reloading: None,
            menu: None,
            clipboard: crate::data::Clipboard::default(),
            activation_serial: None,
            search: None,
            number: None,
            command: None,
            replace: None,
            searches: crate::search::History::default(),
            spelling: crate::spelling::WindowState::default(),
            control_jobs: crate::control_jobs::Jobs::default(),
            control_file_job: None,
            control: None,
            frame_waiters: VecDeque::new(),
            control_cleanup_error: None,
        })
    }

    fn allocate(&mut self, kind: Kind) -> Result<u32> {
        let (id, slot) = self
            .objects
            .iter_mut()
            .enumerate()
            .skip(10)
            .find(|(_, k)| **k == Kind::Free)
            .ok_or("Wayland object budget (waiting for delete_id)")?;
        *slot = kind;
        u32::try_from(id).map_err(error)
    }

    fn kind(&self, id: u32) -> Result<Kind> {
        self.objects
            .get(id as usize)
            .copied()
            .ok_or("unknown Wayland object".into())
    }
    fn set_kind(&mut self, id: u32, kind: Kind) -> Result<()> {
        *self
            .objects
            .get_mut(id as usize)
            .ok_or("unknown Wayland object")? = kind;
        Ok(())
    }

    fn bind(&mut self, name: &str, version: u32, id: u32) -> Result<()> {
        let global = self
            .globals
            .iter()
            .find(|(_, (n, v))| n == name && *v >= version)
            .map(|(id, _)| *id)
            .ok_or_else(|| format!("required Wayland global {name} v{version} is missing"))?;
        let mut body = Builder::new();
        body.u32(global);
        body.string(name)?;
        body.u32(version);
        body.u32(id);
        self.connection.send(REGISTRY, 0, body, None)?;
        self.required.push(global);
        Ok(())
    }

    fn initialize(&mut self) -> Result<()> {
        self.bind("wl_compositor", 4, COMPOSITOR)?;
        self.bind("wl_shm", 1, SHM)?;
        self.bind("xdg_wm_base", 1, WM)?;
        if let Some(version) = self
            .globals
            .values()
            .find(|(name, version)| name == "wl_seat" && *version >= 5)
            .map(|(_, version)| (*version).min(7))
        {
            let seat = self.allocate(Kind::Seat)?;
            self.bind("wl_seat", version, seat)?;
            self.seat = Some(seat);
        } else {
            if self.files.is_some() {
                return Err("file window requires wl_seat v5+".into());
            }
            self.notify("No wl_seat v5+; scratch input unavailable");
        }
        self.connection.words(COMPOSITOR, 0, &[SURFACE])?;
        self.initialize_clipboard()?;
        self.connection.words(WM, 2, &[XDG_SURFACE, SURFACE])?;
        self.connection.words(XDG_SURFACE, 1, &[TOPLEVEL])?;
        for (opcode, value) in [
            (
                2,
                if self.files.is_some() {
                    "td-editor — experimental file window"
                } else {
                    "td-editor — scratch preview (NO SAVE)"
                },
            ),
            (3, "td-editor"),
        ] {
            let mut body = Builder::new();
            body.string(value)?;
            self.connection.send(TOPLEVEL, opcode, body, None)?;
        }
        self.connection.words(SURFACE, 6, &[])?;
        self.bound = true;
        Ok(())
    }

    fn event(&mut self, message: Message) -> Result<()> {
        let result = self.event_inner(message);
        self.cancel_stale_paste();
        self.searches.observe(self.ui.editor());
        self.frames
            .invalidate(self.spelling.observe(self.ui.editor()));
        self.observe_control_jobs();
        result
    }

    fn event_inner(&mut self, message: Message) -> Result<()> {
        if self.clipboard.offers.contains_key(&message.object) {
            return self.clipboard_offer(message);
        }
        let mut cursor = Cursor::new(&message.payload);
        match (message.object, message.opcode) {
            (DISPLAY, 0) => {
                let object = cursor.u32()?;
                let code = cursor.u32()?;
                let detail = cursor.string()?;
                return Err(format!(
                    "Wayland protocol error on object {object}, code {code}: {detail:?}"
                ));
            }
            (DISPLAY, 1) => {
                let id = cursor.u32()?;
                if !matches!(
                    self.kind(id)?,
                    Kind::Retired
                        | Kind::RetiredBuffer
                        | Kind::RetiredKeyboard
                        | Kind::RetiredSeat
                        | Kind::RetiredPointer
                        | Kind::RetiredClipboardDevice
                        | Kind::RetiredClipboardSource
                ) {
                    return Err("unexpected delete_id".into());
                }
                self.set_kind(id, Kind::Free)?;
            }
            (REGISTRY, 0) => {
                let id = cursor.u32()?;
                let name = cursor.string()?;
                let version = cursor.u32()?;
                if id == 0
                    || name.len() > 256
                    || name.contains('\0')
                    || version == 0
                    || self.globals.len() >= 128
                    || self.globals.contains_key(&id)
                {
                    return Err("invalid or excessive Wayland globals".into());
                }
                self.globals.insert(id, (name, version));
            }
            (REGISTRY, 1) => {
                let id = cursor.u32()?;
                cursor.finish()?;
                if self.clipboard.manager.is_some_and(|(name, _)| name == id) {
                    self.release_clipboard()?;
                    self.clipboard.manager = None;
                }
                if self
                    .globals
                    .get(&id)
                    .is_some_and(|(name, _)| name == "wl_seat")
                    && self.required.contains(&id)
                {
                    self.release_keyboard()?;
                    self.release_pointer()?;
                    self.release_clipboard()?;
                    if let Some(seat) = self.seat.take() {
                        self.connection.words(seat, 3, &[])?;
                        self.set_kind(seat, Kind::RetiredSeat)?;
                    }
                    self.required.retain(|global| *global != id);
                    self.globals.remove(&id);
                    self.notify("Seat removed; scratch text retained");
                    return Ok(());
                }
                if self.required.contains(&id) {
                    return Err("required Wayland global was removed".into());
                }
                self.globals.remove(&id);
                return Ok(());
            }
            (SYNC, 0) if !self.bound => {
                cursor.u32()?;
                self.set_kind(SYNC, Kind::Retired)?;
                cursor.finish()?;
                return self.initialize();
            }
            (WM, 0) if self.bound => {
                let serial = cursor.u32()?;
                self.connection.words(WM, 3, &[serial])?;
            }
            (SHM, 0) if self.bound => {
                let format = cursor.u32()?;
                cursor.finish()?;
                self.xrgb |= format == 1;
                self.argb |= format == 0;
                if format == 0 {
                    return self.show_cursor();
                }
                return Ok(());
            }
            (TOPLEVEL, 0) if self.bound => {
                let width = cursor.i32()?;
                let height = cursor.i32()?;
                let length = cursor.u32()?;
                if width < 0 || height < 0 || length > 256 || length % 4 != 0 {
                    return Err("invalid toplevel configure".into());
                }
                for _ in 0..length / 4 {
                    cursor.u32()?;
                }
                self.pending_size = Some((width, height));
            }
            (TOPLEVEL, 1) if self.bound => {
                cursor.finish()?;
                self.close();
                return Ok(());
            }
            (XDG_SURFACE, 0) if self.bound => {
                self.menu = None;
                let serial = cursor.u32()?;
                if let Some((width, height)) = self.pending_size.take() {
                    let current = self.ui.geometry();
                    self.ui
                        .dispatch(Event::Resize {
                            width: if width == 0 {
                                current.dimensions().0
                            } else {
                                width as usize
                            },
                            height: if height == 0 {
                                current.dimensions().1
                            } else {
                                height as usize
                            },
                            scale: 1,
                        })
                        .map_err(|e| format!("Wayland configure geometry: {e}"))?;
                }
                self.connection.words(XDG_SURFACE, 4, &[serial])?;
                self.configured = true;
                self.frames.invalidate(true);
            }
            (SURFACE, 0 | 1) if self.bound => {
                cursor.u32()?;
            }
            (id, opcode) if self.seat == Some(id) || self.kind(id)? == Kind::RetiredSeat => {
                match opcode {
                    0 => {
                        let capabilities = cursor.u32()?;
                        cursor.finish()?;
                        if self.seat != Some(id) {
                            return Ok(());
                        }
                        if capabilities & 1 == 0 {
                            self.release_pointer()?;
                        } else if self.pointer.device.is_none() {
                            let device = self.allocate(Kind::Pointer)?;
                            self.connection.words(id, 0, &[device])?;
                            self.pointer.device = Some(device);
                        }
                        if capabilities & 2 == 0 {
                            self.release_keyboard()?;
                            self.notify("Seat has no keyboard; scratch text retained");
                        } else if self.device.is_none() {
                            let device = self.allocate(Kind::Keyboard)?;
                            self.connection.words(id, 1, &[device])?;
                            self.device = Some(device);
                        }
                        return Ok(());
                    }
                    1 => {
                        if cursor.string()?.len() > 256 {
                            return Err("seat name budget".into());
                        }
                    }
                    _ => return Err("unknown seat event".into()),
                }
            }
            (id, _) if matches!(self.kind(id)?, Kind::Keyboard | Kind::RetiredKeyboard) => {
                return self.keyboard_event(message);
            }
            (id, _) if matches!(self.kind(id)?, Kind::Pointer | Kind::RetiredPointer) => {
                return self.pointer_event(message);
            }
            (id, _)
                if matches!(
                    self.kind(id)?,
                    Kind::ClipboardDevice | Kind::RetiredClipboardDevice
                ) =>
            {
                return self.clipboard_device(message);
            }
            (id, _)
                if matches!(
                    self.kind(id)?,
                    Kind::ClipboardSource | Kind::RetiredClipboardSource
                ) =>
            {
                return self.clipboard_source(message);
            }
            (id, 0) if self.kind(id)? == Kind::ClipboardSync => {
                cursor.u32()?;
                cursor.finish()?;
                let retired = self
                    .clipboard
                    .barriers
                    .remove(&id)
                    .ok_or("missing clipboard barrier")?;
                for (offer, sequence) in retired {
                    if self
                        .clipboard
                        .offers
                        .get(&offer)
                        .is_some_and(|o| o.retired && o.sequence == sequence)
                    {
                        self.clipboard.offers.remove(&offer);
                    }
                }
                self.set_kind(id, Kind::Retired)?;
                return self.queue_offer_barrier();
            }
            (id, 0 | 1) if self.kind(id)? == Kind::CursorSurface => {
                cursor.u32()?;
            }
            (id, 0) if self.kind(id)? == Kind::CursorBuffer => {
                cursor.finish()?;
                let image = self
                    .cursor_image
                    .as_mut()
                    .filter(|image| image.buffer == id && image.busy)
                    .ok_or("unexpected cursor buffer release")?;
                image.busy = false;
                return Ok(());
            }
            (id, 0) if self.kind(id)? == Kind::Frame && self.callback == Some(id) => {
                cursor.u32()?;
                self.frames.complete().map_err(error)?;
                self.callback = None;
                self.set_kind(id, Kind::Retired)?;
            }
            (id, 0) if self.kind(id)? == Kind::Buffer => {
                let buffer = self
                    .buffers
                    .iter_mut()
                    .find(|b| b.id == id)
                    .ok_or("missing buffer")?;
                if !buffer.busy {
                    return Err("duplicate buffer release".into());
                }
                buffer.busy = false;
            }
            (id, 0) if self.kind(id)? == Kind::RetiredBuffer => {}
            _ => {
                return Err(format!(
                    "unexpected Wayland event {}:{}",
                    message.object, message.opcode
                ))
            }
        }
        cursor.finish()
    }

    fn notify(&mut self, detail: impl AsRef<str>) {
        self.notice = Some(
            detail
                .as_ref()
                .chars()
                .take(512)
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect(),
        );
        self.frames.invalidate(true);
    }

    fn close(&mut self) {
        self.search = None;
        self.number = None;
        self.command = None;
        self.replace = None;
        self.searches.cancel_wrap();
        self.clipboard.incoming = None;
        self.menu = None;
        self.stop_pointer();
        self.input.cancel_repeat();
        if self.files.is_some() {
            let _ = self.start_close(Scope::Window);
            return;
        }
        self.prompt = None;
        if self.ui.editor().tabs().any(|(_, doc)| doc.dirty()) {
            self.quitting = true;
            self.frames.invalidate(true);
        } else {
            self.closed = true;
        }
    }

    fn release_keyboard(&mut self) -> Result<()> {
        self.searches.cancel_wrap();
        self.clipboard_focus_lost()?;
        self.menu = None;
        if let Some(device) = self.device.take() {
            self.connection.words(device, 0, &[])?;
            self.set_kind(device, Kind::RetiredKeyboard)?;
        }
        self.input = Input::default();
        self.ui.dispatch(Event::Focus(false)).map_err(error)?;
        self.frames.invalidate(true);
        Ok(())
    }

    fn release_pointer(&mut self) -> Result<()> {
        if let Some(device) = self.pointer.device {
            self.connection.words(device, 1, &[])?;
            self.set_kind(device, Kind::RetiredPointer)?;
        }
        self.pointer = Pointer::default();
        self.ui.dispatch(Event::CancelPointer).map_err(error)?;
        Ok(())
    }

    fn pointer_modal(&self) -> bool {
        self.quitting
            || self.search.is_some()
            || self.number.is_some()
            || self.command.is_some()
            || self.replace.is_some()
            || self.prompt.is_some()
            || self.closing.is_some()
            || self.conflict.is_some()
            || self.reloading.is_some()
    }

    fn clear_pointer_gesture(&mut self) {
        self.pointer.held = false;
        self.pointer.wheel = crate::pointer::Wheel::default();
        self.pointer.wheel_target = None;
        self.pointer.wheel_context = None;
    }

    fn stop_pointer(&mut self) {
        self.clear_pointer_gesture();
        if let Err(e) = self.ui.dispatch(Event::CancelPointer) {
            self.notify(format!("Pointer reset refused: {e}"));
        }
    }

    fn pointer_event(&mut self, message: Message) -> Result<()> {
        use crate::pointer::Event as P;
        let event = crate::pointer::decode(&message)?;
        if self.pointer.device != Some(message.object) {
            return Ok(());
        }
        if let P::Enter { surface, .. } | P::Leave(surface) = event {
            if surface != SURFACE {
                return Err("pointer event for unknown surface".into());
            }
        }
        if self.pointer_modal() {
            self.stop_pointer();
        }
        match event {
            P::Enter { serial, x, y, .. } => {
                self.pointer.enter = Some(serial);
                self.pointer.x = x;
                self.pointer.y = y;
                self.stop_pointer();
                self.show_cursor()?;
            }
            P::Leave(_) => {
                self.frames.invalidate(self.menu.take().is_some());
                self.pointer.enter = None;
                self.stop_pointer();
            }
            P::Motion(x, y) => {
                self.pointer.x = x;
                self.pointer.y = y;
                if let Some(menu) = &mut self.menu {
                    if let Some(index) = menu.hit(
                        self.ui.geometry(),
                        i64::from(x).div_euclid(256),
                        i64::from(y).div_euclid(256),
                    ) {
                        if menu
                            .group
                            .items()
                            .get(index)
                            .is_some_and(|item| menu.enabled(*item))
                        {
                            self.frames.invalidate(menu.selected != index);
                            menu.selected = index;
                        }
                    }
                } else if self.pointer.held {
                    self.pointer_action(crate::ui::PointerPhase::Move)?;
                }
            }
            P::Button {
                button: 0x110,
                pressed,
                serial,
            } if self.pointer.enter.is_some() && !self.pointer_modal() => {
                if pressed != self.pointer.held {
                    self.pointer.held = pressed;
                    self.activation_serial = pressed.then_some(serial);
                    self.pointer_action(if pressed {
                        crate::ui::PointerPhase::Press
                    } else {
                        crate::ui::PointerPhase::Release
                    })?;
                    self.activation_serial = None;
                }
            }
            P::Axis(..) | P::Source(_) | P::Stop(_) | P::Discrete(..)
                if self.pointer.enter.is_some() && !self.pointer_modal() && self.menu.is_none() =>
            {
                if self.pointer.wheel_target.is_none() {
                    let target = self.ui.editor().active().and_then(|tab| {
                        self.ui.editor().document(tab).ok().map(|doc| Target {
                            tab,
                            revision: doc.revision(),
                        })
                    });
                    if target != self.pointer.wheel_context {
                        self.pointer.wheel = crate::pointer::Wheel::default();
                        self.pointer.wheel_context = target;
                    }
                    self.pointer.wheel_target = target;
                }
                self.pointer.wheel.update(event)?;
            }
            P::Frame => {
                let (rows, columns) = self.pointer.wheel.frame();
                if let Some(Target { tab, revision }) = self.pointer.wheel_target.take() {
                    if (rows != 0 || columns != 0)
                        && !self.pointer_modal()
                        && self.ui.editor().active() == Some(tab)
                    {
                        let before = self.ui.generation();
                        match self.ui.dispatch(Event::Scroll {
                            tab,
                            revision,
                            rows,
                            columns,
                        }) {
                            Ok(_) | Err(crate::Error::MissingTab | crate::Error::StaleRevision) => {
                            }
                            Err(e) => self.notify(format!("Scroll refused: {e}")),
                        }
                        self.frames.invalidate(before != self.ui.generation());
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn pointer_action(&mut self, phase: crate::ui::PointerPhase) -> Result<()> {
        let raw_x = i64::from(self.pointer.x).div_euclid(256);
        let raw_y = i64::from(self.pointer.y).div_euclid(256);
        if phase == crate::ui::PointerPhase::Press && self.menu_pointer(raw_x, raw_y)? {
            self.pointer.held = false;
            return Ok(());
        }
        if self.menu.is_some() {
            return Ok(());
        }
        let Some(tab) = self.ui.editor().active() else {
            return Ok(());
        };
        let revision = self.ui.editor().document(tab).map_err(error)?.revision();
        let geometry = self.ui.geometry();
        let x = i64::from(self.pointer.x).div_euclid(256);
        let y = i64::from(self.pointer.y).div_euclid(256);
        let area = geometry.document();
        // Keep chrome hit boxes on integer pixels; inside text preserve strict
        // midpoint ties even for signed 24.8 subpixel coordinates.
        let text = x >= area.x
            && x < area.x + i64::from(area.width)
            && y >= area.y
            && y < area.y + i64::from(area.height);
        let x = if text || phase != crate::ui::PointerPhase::Press {
            let ceil = x + i64::from(self.pointer.x.rem_euclid(256) != 0);
            if text {
                ceil.min(area.x + i64::from(area.width) - 1)
            } else {
                ceil
            }
        } else {
            x
        };
        let extend = self.input.focused
            && self.input.synchronized
            && self
                .input
                .map
                .as_ref()
                .is_some_and(|map| map.pointer_extend(self.input.modifiers));
        let before = self.ui.generation();
        let outcome = match self.ui.dispatch(Event::Pointer {
            tab,
            revision,
            phase,
            x,
            y,
            extend,
        }) {
            Ok(outcome) => outcome,
            Err(detail) => {
                self.stop_pointer();
                self.notify(format!("Pointer refused: {detail}"));
                return Ok(());
            }
        };
        self.frames.invalidate(before != self.ui.generation());
        if !matches!(outcome, Outcome::Ignored) {
            self.input.cancel_repeat();
        }
        if let Outcome::Request {
            name: "close-tab",
            tab,
            revision,
        } = outcome
        {
            self.close_tab(tab, revision);
        }
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<()> {
        let (Some(device), Some(serial)) = (self.pointer.device, self.pointer.enter) else {
            return Ok(());
        };
        if !self.argb {
            return Ok(());
        }
        if self.cursor_image.is_none() {
            let file = backing_file(&self.temporary, 16 * 24 * 4)?;
            file.write_all_at(&cursor_pixels(), 0).map_err(error)?;
            let surface = self.allocate(Kind::CursorSurface)?;
            let pool = self.allocate(Kind::Pool)?;
            let buffer = self.allocate(Kind::CursorBuffer)?;
            self.connection.words(COMPOSITOR, 0, &[surface])?;
            let mut body = Builder::new();
            body.u32(pool);
            body.u32(16 * 24 * 4);
            self.connection.send(SHM, 0, body, Some(&file))?;
            self.connection
                .words(pool, 0, &[buffer, 0, 16, 24, 64, 0])?;
            self.connection.words(pool, 1, &[])?;
            self.set_kind(pool, Kind::Retired)?;
            self.connection.words(surface, 1, &[buffer, 0, 0])?;
            self.connection.words(surface, 2, &[0, 0, 16, 24])?;
            self.connection.words(surface, 6, &[])?;
            self.cursor_image = Some(CursorImage {
                surface,
                buffer,
                busy: true,
            });
        }
        let image = self.cursor_image.as_ref().ok_or("cursor image missing")?;
        self.connection
            .words(device, 0, &[serial, image.surface, 0, 0])
    }

    fn keyboard_event(&mut self, message: Message) -> Result<()> {
        let event = keyboard_event(&message)?;
        let active = self.device == Some(message.object);
        // Retired objects can have events in flight, including rights. Drain
        // their exact schema until delete_id without activating a new map.
        if let KeyboardEvent::Map(format, size) = event {
            let fd = self
                .connection
                .descriptors
                .pop_front()
                .ok_or("missing keymap descriptor")?;
            if !active {
                return Ok(());
            }
            self.menu = None;
            self.clipboard.incoming = None;
            self.input.map = None;
            self.searches.cancel_wrap();
            self.input.cancel_repeat();
            self.input.synchronized = false;
            self.ui.dispatch(Event::Focus(false)).map_err(error)?;
            match read_keymap(fd, format, size) {
                Ok(map) => {
                    self.input.map = Some(map);
                    self.ui
                        .dispatch(Event::Focus(self.input.focused))
                        .map_err(error)?;
                    self.notify(
                        "Keymap compiled; waiting for focus/modifier snapshot. Tap and release Shift if needed.",
                    );
                }
                Err(detail) => self.notify(format!("Keyboard disabled: {detail}")),
            }
            self.frames.invalidate(true);
            return Ok(());
        }
        if !active {
            return Ok(());
        }
        match event {
            KeyboardEvent::Map(_, _) => {}
            KeyboardEvent::Enter(surface, keys) => {
                if surface != SURFACE {
                    return Err("keyboard enter for unknown surface".into());
                }
                self.input.focus(&keys, true)?;
                self.ui
                    .dispatch(Event::Focus(self.input.map.is_some()))
                    .map_err(error)?;
                self.frames.invalidate(true);
            }
            KeyboardEvent::Leave(surface) => {
                if surface != SURFACE {
                    return Err("keyboard leave for unknown surface".into());
                }
                self.input.focus(&[], false)?;
                self.searches.cancel_wrap();
                self.clipboard_focus_lost()?;
                self.menu = None;
                self.ui.dispatch(Event::Focus(false)).map_err(error)?;
                self.frames.invalidate(true);
            }
            KeyboardEvent::Modifiers(modifiers) => {
                let ready =
                    !self.input.synchronized && self.input.focused && self.input.map.is_some();
                self.input.modifiers(modifiers);
                if ready {
                    self.notify(if self.files.is_some() {
                        "Keymap ready. Experimental file window; Escape dismisses."
                    } else {
                        "Keymap ready; Escape dismisses this notice. Scratch only: no Save."
                    });
                }
            }
            KeyboardEvent::Timing(rate, delay) => self.input.timing(rate, delay, self.clock)?,
            KeyboardEvent::Key(serial, key, pressed) => match self.input.key(key, pressed) {
                Ok(Some(stroke)) => {
                    self.activation_serial = Some(serial);
                    if self.chord(&stroke.chord, false)? && stroke.repeat {
                        self.input.arm(key, self.clock);
                    }
                    self.activation_serial = None;
                }
                Ok(None) => {}
                Err(detail) => self.notify(detail),
            },
        }
        Ok(())
    }

    fn chord(&mut self, chord: &str, repeated: bool) -> Result<bool> {
        if matches!(chord, "Escape" | "C-g") && !repeated {
            self.clipboard.incoming = None;
            self.searches.cancel_wrap();
            self.frames.invalidate(self.spelling.cancel());
        }
        if self.quitting {
            if !repeated {
                match chord {
                    "C-d" => self.closed = true,
                    "Escape" | "C-g" => {
                        self.quitting = false;
                        self.frames.invalidate(true);
                    }
                    _ => {}
                }
            }
            return Ok(false);
        }
        if self.prompt.is_some() {
            self.path_chord(chord, repeated);
            return Ok(false);
        }
        if self.closing.is_some() {
            self.close_chord(chord, repeated);
            return Ok(false);
        }
        if self.conflict.is_some() || self.reloading.is_some() {
            self.conflict_chord(chord, repeated);
            return Ok(false);
        }
        if self.number.is_some() {
            self.number_chord(chord, repeated)?;
            return Ok(false);
        }
        if self.command.is_some() {
            self.command_chord(chord, repeated)?;
            return Ok(false);
        }
        if self.search.is_some() {
            self.search_chord(chord, repeated)?;
            return Ok(false);
        }
        if self.replace.is_some() {
            self.replace_chord(chord, repeated)?;
            return Ok(false);
        }
        if self.menu.is_some() {
            self.menu_chord(chord, repeated)?;
            return Ok(false);
        }
        if chord == "F10" {
            if !repeated {
                self.open_menu(crate::menu::Group::File)?;
            }
            return Ok(false);
        }
        if matches!(chord, "Escape" | "C-g") && self.notice.take().is_some() {
            self.frames.invalidate(true);
        }
        let Some(tab) = self.ui.editor().active() else {
            return Ok(false);
        };
        let revision = self.ui.editor().document(tab).map_err(error)?.revision();
        if chord == "F7" {
            if !repeated {
                // The shared action already renders startup feedback.
                let _ = self.spelling_request(tab, revision);
            }
            return Ok(false);
        }
        if chord == "F6" {
            if !repeated {
                self.number_request(tab, revision, crate::number::Kind::Line)?;
            }
            return Ok(false);
        }
        let before = self.ui.generation();
        let result = self.ui.dispatch(Event::Key {
            tab,
            revision,
            chord,
        });
        self.frames.invalidate(self.ui.generation() != before);
        match result {
            Ok(Outcome::Changed) => Ok(true),
            Ok(Outcome::Request { name: "quit", .. }) => {
                self.close();
                Ok(false)
            }
            Ok(Outcome::Request {
                name: "close-tab",
                tab,
                revision,
            }) => {
                self.close_tab(tab, revision);
                Ok(false)
            }
            Ok(Outcome::Request {
                name: "command-prompt",
                tab,
                revision,
            }) => {
                if !repeated {
                    self.command_request(tab, revision)?;
                }
                Ok(false)
            }
            Ok(Outcome::Request {
                name: "replace",
                tab,
                revision,
            }) => {
                if !repeated {
                    self.replace_request(tab, revision)?;
                }
                Ok(false)
            }
            Ok(Outcome::Request {
                name,
                tab,
                revision,
            }) if matches!(
                name,
                "find" | "find-backward" | "find-next" | "find-previous"
            ) =>
            {
                if !repeated {
                    self.search_request(name, tab, revision)?;
                }
                Ok(false)
            }
            Ok(Outcome::Request {
                name,
                tab,
                revision,
            }) if matches!(name, "cut" | "copy" | "paste") => {
                if !repeated {
                    self.clipboard_request(name, tab, revision)?;
                }
                Ok(false)
            }
            Ok(Outcome::Request {
                name,
                tab,
                revision,
            }) if self.files.is_some() && matches!(name, "open" | "save" | "save-as") => {
                self.file_request(name, tab, revision);
                Ok(false)
            }
            Ok(Outcome::Request { name, .. }) => {
                self.notify(format!(
                    "{name} is not connected in this {}. Escape dismisses.",
                    if self.files.is_some() {
                        "experimental window"
                    } else {
                        "scratch preview"
                    }
                ));
                Ok(false)
            }
            Ok(_) => Ok(false),
            Err(detail) => {
                self.notify(format!("Key refused: {detail}"));
                Ok(false)
            }
        }
    }

    fn tick(&mut self, now: u64, repeat: bool) -> Result<()> {
        let active = self.ui.editor().active();
        if let Some(result) = self
            .files
            .as_mut()
            .and_then(|files| files.poll(&mut self.ui))
        {
            let mut job_error = None;
            if let Some(job) = self.control_file_job.take() {
                // Session completes the single file job on this UI thread.
                let outcome = match job {
                    ControlFile::Open(id) => {
                        // Open success follows selection or creation of its tab.
                        let opened = result.as_ref().map_err(|error| error.code).and_then(|_| {
                            self.ui
                                .editor()
                                .active()
                                .ok_or(crate::Error::MissingTab)
                                .and_then(|tab| {
                                    Ok(Target {
                                        tab,
                                        revision: self.ui.editor().document(tab)?.revision(),
                                    })
                                })
                        });
                        self.control_jobs.opened(id, opened)
                    }
                    ControlFile::Save(id) => self
                        .control_jobs
                        .saved(id, result.as_ref().map(|_| ()).map_err(|error| error.code)),
                    ControlFile::Reload(id) => self.control_jobs.reloaded(
                        id,
                        result
                            .as_ref()
                            .map(|_| ReloadOutcome::Complete)
                            .map_err(|error| error.code),
                    ),
                    ControlFile::Dictionary(id) => self
                        .control_jobs
                        .dictionary(id, result.as_ref().map(|_| ()).map_err(|error| error.code)),
                };
                if let Err(detail) = outcome {
                    job_error = Some(detail);
                }
                self.frames.invalidate(true);
            }
            self.menu = None;
            if self.ui.editor().active() != active {
                self.input.cancel_repeat();
            }
            let succeeded = result.is_ok();
            if let Some(dictionary) = self
                .files
                .as_mut()
                .and_then(|files| files.take_dictionary())
            {
                self.spelling.install(dictionary);
                self.frames.invalidate(true);
            }
            let mut notice = match result {
                Ok(notice) => notice,
                Err(error) => format!("File operation failed: {}", error.detail),
            };
            if let Some(detail) = job_error {
                notice.push_str(&format!("; File job outcome unavailable: {detail}"));
            }
            self.notify(notice);
            if self.closing_save {
                self.closing_save = false;
                if succeeded {
                    let _ = self.advance_close();
                } else {
                    self.close_failed();
                }
            }
            self.reloading = None;
            if let Some(target) = self.files.as_mut().and_then(|files| files.take_conflict()) {
                self.input.cancel_repeat();
                let prepared = self
                    .last_dialog_id
                    .checked_add(1)
                    .ok_or(crate::Error::Exhausted)
                    .and_then(|id| {
                        Conflict::new(self.ui.editor(), target).map(|conflict| (id, conflict))
                    });
                match prepared {
                    Ok((id, conflict)) => {
                        self.stop_pointer();
                        self.last_dialog_id = id;
                        self.conflict = Some(conflict);
                    }
                    Err(detail) => self.notify(format!("Conflict question unavailable: {detail}")),
                }
            }
        }
        let before = self.ui.generation();
        self.ui.dispatch(Event::Tick(now)).map_err(error)?;
        self.clock = now;
        self.clipboard_tick(now, repeat)?;
        self.frames.invalidate(self.ui.generation() != before);
        if repeat {
            match self.input.repeat(now) {
                Ok(Some(stroke)) => {
                    if !self.chord(&stroke.chord, true)? {
                        self.input.cancel_repeat();
                    }
                }
                Ok(None) => {}
                Err(detail) => {
                    self.input.cancel_repeat();
                    self.notify(detail);
                }
            }
        }
        self.connection.wait = Duration::from_millis(self.input.wait_ms(now));
        if self.clipboard.incoming.is_some() || self.clipboard.outgoing.is_some() {
            self.connection.wait = self.connection.wait.min(Duration::from_millis(10));
        }
        self.cancel_stale_paste();
        self.searches.observe(self.ui.editor());
        self.frames
            .invalidate(self.spelling.observe(self.ui.editor()));
        Ok(())
    }

    fn end_turn(&mut self, now: u64, repeat: bool) -> Result<()> {
        self.tick(now, repeat)?;
        match self.spelling.step(self.ui.editor()) {
            Ok(changed) => self.frames.invalidate(changed),
            Err(detail) => self.notify(format!("Spelling cancelled: {detail}")),
        }
        if self.spelling.running() {
            self.connection.wait = self.connection.wait.min(Duration::from_millis(1));
        }
        self.observe_control_jobs();
        self.control_tick();
        self.frames.generation().map_err(error)?;
        Ok(())
    }

    fn control_tick(&mut self) {
        if self.closed {
            return;
        }
        if self.control.is_some() {
            self.connection.wait = self.connection.wait.min(Duration::from_millis(10));
        }
        let mut budget = CONTROL_JOBS_PER_TURN;
        // Inspect each held job once. Replies share the ordinary admission budget.
        for _ in 0..self.frame_waiters.len() {
            let Some(job) = self.frame_waiters.pop_front() else {
                break;
            };
            if !job.is_live() {
                continue;
            }
            let ready = match job.request().operation {
                crate::control::Operation::WaitFrame(target) => {
                    self.frames.wait(target) != Ok(None)
                }
                // Only waits are queued; unexpected jobs still use ordinary admission.
                _ => true,
            };
            if ready && budget > 0 {
                budget -= 1;
                if let Err(detail) = job.respond_with(|request| self.control_response(request)) {
                    self.notify(format!("Control response refused: {detail}"));
                }
            } else {
                self.frame_waiters.push_back(job);
            }
        }
        // One outer turn, not every decoded Wayland event, budgets UI work.
        for _ in 0..CONTROL_JOBS_PER_TURN {
            if budget == 0 {
                break;
            }
            let Some(worker) = self.control.as_ref() else {
                break;
            };
            let job = match worker.try_request() {
                Ok(Some(job)) => job,
                Ok(None) => break,
                Err(detail) => {
                    self.disable_control(&detail.to_string());
                    break;
                }
            };
            if !job.is_live() {
                continue;
            }
            budget -= 1;
            if let crate::control::Operation::WaitFrame(target) = job.request().operation {
                if self.frames.wait(target) == Ok(None) {
                    if self.frame_waiters.len() < crate::control_worker::CONNECTIONS {
                        self.frame_waiters.push_back(job);
                    } else if let Err(detail) = job.respond_with(|request| {
                        crate::control::Refusal {
                            id: request.id,
                            error: crate::Error::Limit,
                        }
                        .response()
                    }) {
                        self.notify(format!("Control response refused: {detail}"));
                    }
                    continue;
                }
            }
            if let Err(detail) = job.respond_with(|request| self.control_response(request)) {
                self.notify(format!("Control response refused: {detail}"));
            }
        }
    }

    fn control_response(&mut self, request: &crate::control::Request) -> String {
        if let Err(error) = self.frames.generation() {
            return crate::control::Refusal {
                id: request.id,
                error,
            }
            .response();
        }
        if let crate::control::Operation::DialogAnswer {
            dialog,
            tab,
            revision,
            answer,
        } = &request.operation
        {
            return match self.control_dialog_answer(
                *dialog,
                Target {
                    tab: *tab,
                    revision: *revision,
                },
                answer,
            ) {
                Ok(ControlAnswer::Done) => format!("1\t{}\tok\t", request.id),
                Ok(ControlAnswer::Dialog(id)) => format!("1\t{}\tok\tdialog\t{id}", request.id),
                Ok(ControlAnswer::Job(id)) => format!("1\t{}\tpending\t{id}", request.id),
                Err(error) => crate::control::Refusal {
                    id: request.id,
                    error,
                }
                .response(),
            };
        }
        if request.is_mutating() && (self.closed || self.pointer_modal() || self.menu.is_some()) {
            return crate::control::Refusal {
                id: request.id,
                error: crate::Error::Unavailable,
            }
            .response();
        }
        if matches!(
            request.operation,
            crate::control::Operation::CloseTab { .. } | crate::control::Operation::Quit
        ) {
            let result = (|| -> crate::Result<String> {
                if self.files.is_none() || self.files.as_ref().is_some_and(|files| files.busy()) {
                    return Err(crate::Error::Unavailable);
                }
                let scope = match request.operation {
                    crate::control::Operation::CloseTab { tab, revision } => {
                        self.ui.editor().revision_point(tab, revision)?;
                        if self.ui.editor().active() != Some(tab) {
                            return Err(crate::Error::InvalidArgument);
                        }
                        Scope::Tab { tab, revision }
                    }
                    crate::control::Operation::Quit => Scope::Window,
                    _ => return Err(crate::Error::InvalidArgument),
                };
                // Native start_close also checks this, after its input cleanup.
                self.last_dialog_id
                    .checked_add(1)
                    .ok_or(crate::Error::Exhausted)?;
                // Pointer cancellation and clean-tab completion can each dispatch once.
                self.ui
                    .generation()
                    .checked_add(2)
                    .ok_or(crate::Error::Exhausted)?;
                self.start_close(scope)?;
                self.control_mutation_accepted();
                Ok(if self.closing.is_some() {
                    format!("dialog\t{}", self.last_dialog_id)
                } else {
                    "closed".into()
                })
            })();
            return match result {
                Ok(body) => format!("1\t{}\tok\t{body}", request.id),
                Err(error) => crate::control::Refusal {
                    id: request.id,
                    error,
                }
                .response(),
            };
        }
        if let crate::control::Operation::Save {
            tab,
            revision,
            path,
        } = &request.operation
        {
            let result = (|| -> crate::Result<u64> {
                if self.files.is_none()
                    || self.files.as_ref().is_some_and(|files| files.busy())
                    || self.control_file_job.is_some()
                {
                    return Err(crate::Error::Unavailable);
                }
                self.ui.editor().revision_point(*tab, *revision)?;
                if self.ui.editor().active() != Some(*tab)
                    || (path.is_none()
                        && !self
                            .files
                            .as_ref()
                            .is_some_and(|files| files.associated(*tab)))
                {
                    return Err(crate::Error::InvalidArgument);
                }
                self.control_save_job(
                    Target {
                        tab: *tab,
                        revision: *revision,
                    },
                    path.clone(),
                )
            })();
            return match result {
                Ok(id) => format!("1\t{}\tpending\t{id}", request.id),
                Err(error) => crate::control::Refusal {
                    id: request.id,
                    error,
                }
                .response(),
            };
        }
        if let crate::control::Operation::Open(path) = &request.operation {
            let result = self.control_open_job(path.clone());
            return match result {
                Ok(id) => format!("1\t{}\tpending\t{id}", request.id),
                Err(error) => crate::control::Refusal {
                    id: request.id,
                    error,
                }
                .response(),
            };
        }
        if matches!(request.operation, crate::control::Operation::New) {
            return match self.ui.dispatch(Event::New) {
                Ok(Outcome::Created(tab)) => {
                    self.control_mutation_accepted();
                    format!("1\t{}\tok\t{tab}", request.id)
                }
                Ok(_) => {
                    // Defensive against a changed controller outcome, not bad wire input.
                    self.control_mutation_accepted();
                    crate::control::Refusal {
                        id: request.id,
                        error: crate::Error::Unavailable,
                    }
                    .response()
                }
                Err(error) => crate::control::Refusal {
                    id: request.id,
                    error,
                }
                .response(),
            };
        }
        if let crate::control::Operation::CheckSpelling { tab, revision } = request.operation {
            let result = (|| -> crate::Result<u64> {
                self.ui.editor().revision_point(tab, revision)?;
                if self.ui.editor().active() != Some(tab) {
                    return Err(crate::Error::InvalidArgument);
                }
                let id = self.control_jobs.begin(tab, revision)?;
                let started = self.spelling_request(tab, revision);
                // No intervening admission: this fresh pending row cannot be evicted.
                self.control_jobs.started(id, started)?;
                self.observe_control_jobs();
                self.frames.invalidate(true);
                Ok(id)
            })();
            return match result {
                Ok(id) => format!("1\t{}\tpending\t{id}", request.id),
                Err(error) => crate::control::Refusal {
                    id: request.id,
                    error,
                }
                .response(),
            };
        }
        if let crate::control::Operation::WaitFrame(target) = request.operation {
            return match self.frames.wait(target) {
                Ok(Some(stamp)) => format!("1\t{}\tok\t{}", request.id, stamp.fields()),
                // The scheduler holds pending waits; direct dispatch still fails safely.
                Ok(None) => crate::control::Refusal {
                    id: request.id,
                    error: crate::Error::Unavailable,
                }
                .response(),
                Err(error) => crate::control::Refusal {
                    id: request.id,
                    error,
                }
                .response(),
            };
        }
        if matches!(
            request.operation,
            crate::control::Operation::SpellingResults { .. }
        ) {
            return request.spelling_response(&self.ui, &self.spelling);
        }
        if request.is_edit() {
            let result = request.execute(&mut self.ui);
            return match result {
                Ok(()) => {
                    self.control_mutation_accepted();
                    format!("1\t{}\tok\t", request.id)
                }
                Err(error) => request.admitted_edit_refusal(error),
            };
        }
        let response = request.response(&self.ui);
        if request.operation == crate::control::Operation::State {
            self.native_state(response).unwrap_or_else(|error| {
                crate::control::Refusal {
                    id: request.id,
                    error,
                }
                .response()
            })
        } else {
            response
        }
    }

    fn control_open_job(&mut self, path: PathBuf) -> crate::Result<u64> {
        if self.files.is_none()
            || self.files.as_ref().is_some_and(|files| files.busy())
            || self.control_file_job.is_some()
        {
            return Err(crate::Error::Unavailable);
        }
        self.ui
            .generation()
            .checked_add(1)
            .ok_or(crate::Error::Exhausted)?;
        let files = self.files.as_mut().ok_or(crate::Error::Unavailable)?;
        let id = self.control_jobs.begin_open()?;
        match files.open(path) {
            Ok(()) => self.control_file_job = Some(ControlFile::Open(id)),
            Err(detail) => {
                self.control_jobs
                    .opened(id, Err(crate::Error::Unavailable))?;
                self.notify(format!("Open failed: {detail}"));
            }
        }
        self.stop_pointer();
        self.control_mutation_accepted();
        Ok(id)
    }

    fn control_dictionary_job(&mut self, path: PathBuf) -> crate::Result<u64> {
        if self.files.is_none()
            || self.files.as_ref().is_some_and(|files| files.busy())
            || self.control_file_job.is_some()
        {
            return Err(crate::Error::Unavailable);
        }
        self.ui
            .generation()
            .checked_add(1)
            .ok_or(crate::Error::Exhausted)?;
        let files = self.files.as_mut().ok_or(crate::Error::Unavailable)?;
        let id = self.control_jobs.begin_dictionary()?;
        match files.dictionary(path) {
            Ok(()) => self.control_file_job = Some(ControlFile::Dictionary(id)),
            Err(detail) => {
                self.control_jobs
                    .dictionary(id, Err(crate::Error::Unavailable))?;
                self.notify(format!("Dictionary failed: {detail}"));
            }
        }
        self.stop_pointer();
        self.control_mutation_accepted();
        Ok(id)
    }

    fn control_save_job(&mut self, target: Target, path: Option<PathBuf>) -> crate::Result<u64> {
        if self.files.is_none()
            || self.files.as_ref().is_some_and(|files| files.busy())
            || self.control_file_job.is_some()
        {
            return Err(crate::Error::Unavailable);
        }
        self.ui
            .editor()
            .revision_point(target.tab, target.revision)?;
        self.ui
            .generation()
            .checked_add(1)
            .ok_or(crate::Error::Exhausted)?;
        let files = self.files.as_mut().ok_or(crate::Error::Unavailable)?;
        let id = self
            .control_jobs
            .begin_save(target.tab, target.revision, path.is_some())?;
        match files.queue_save(&self.ui, target.tab, target.revision, path) {
            Ok(()) => self.control_file_job = Some(ControlFile::Save(id)),
            Err(detail) => {
                self.control_jobs
                    .saved(id, Err(crate::Error::Unavailable))?;
                self.notify(format!("Save failed: {detail}"));
            }
        }
        self.stop_pointer();
        self.control_mutation_accepted();
        Ok(id)
    }

    fn control_dialog_answer(
        &mut self,
        dialog: u64,
        target: Target,
        answer: &crate::control::DialogAnswer,
    ) -> crate::Result<ControlAnswer> {
        if self.closed || self.files.is_none() {
            return Err(crate::Error::Unavailable);
        }
        let Some(close) = self.closing.as_ref() else {
            if self.conflict.is_none() && self.reloading.is_none() {
                return self.control_path_answer(dialog, target, answer);
            }
            return self.control_conflict_answer(dialog, target, answer);
        };
        if dialog == 0 || dialog != self.last_dialog_id {
            return Err(crate::Error::InvalidArgument);
        }
        let current = close
            .next(self.ui.editor())?
            .ok_or(crate::Error::Unavailable)?;
        if current.tab != target.tab {
            return Err(crate::Error::InvalidArgument);
        }
        if current.revision != target.revision {
            return Err(crate::Error::StaleRevision);
        }
        match answer {
            crate::control::DialogAnswer::Cancel => self.cancel_close(),
            crate::control::DialogAnswer::Discard => {
                if self.closing_save
                    || self.prompt.is_some()
                    || self.files.as_ref().is_some_and(|files| files.busy())
                    || self.control_file_job.is_some()
                {
                    return Err(crate::Error::Unavailable);
                }
                self.ui
                    .generation()
                    .checked_add(1)
                    .ok_or(crate::Error::Exhausted)?;
                self.discard_close(target)?;
            }
            crate::control::DialogAnswer::Save => {
                if self.closing_save
                    || self.prompt.is_some()
                    || self.files.as_ref().is_some_and(|files| files.busy())
                    || self.control_file_job.is_some()
                {
                    return Err(crate::Error::Unavailable);
                }
                if self
                    .files
                    .as_ref()
                    .is_some_and(|files| files.associated(target.tab))
                {
                    return self.control_close_save_job(target, None);
                }
                self.ui
                    .generation()
                    .checked_add(1)
                    .ok_or(crate::Error::Exhausted)?;
                // The ordinary close Save flow asks for a path for untitled tabs.
                if !self.file_request("save", target.tab, target.revision) {
                    self.close_failed();
                    return Err(crate::Error::Unavailable);
                }
                self.control_mutation_accepted();
                return Ok(ControlAnswer::Dialog(dialog));
            }
            crate::control::DialogAnswer::Path(path) => {
                if self.closing_save
                    || !self.prompt.as_ref().is_some_and(|prompt| {
                        matches!(prompt.action, PathAction::Save { tab, revision }
                        if tab == target.tab && revision == target.revision)
                    })
                {
                    return Err(crate::Error::Unavailable);
                }
                return self.control_close_save_job(target, Some(path.clone()));
            }
            crate::control::DialogAnswer::Reload
            | crate::control::DialogAnswer::DiscardReload
            | crate::control::DialogAnswer::SaveAs(_) => return Err(crate::Error::Unavailable),
        }
        self.control_mutation_accepted();
        Ok(ControlAnswer::Done)
    }

    fn control_path_answer(
        &mut self,
        dialog: u64,
        target: Target,
        answer: &crate::control::DialogAnswer,
    ) -> crate::Result<ControlAnswer> {
        let prompt = self.prompt.as_ref().ok_or(crate::Error::Unavailable)?;
        let identity = prompt.identity.as_ref().ok_or(crate::Error::Unavailable)?;
        if dialog == 0 || dialog != identity.id || target.tab != identity.point.tab {
            return Err(crate::Error::InvalidArgument);
        }
        if target.revision != identity.point.revision {
            return Err(crate::Error::StaleRevision);
        }
        self.ui.editor().check_revision(&identity.point)?;
        let path = match answer {
            crate::control::DialogAnswer::Cancel => {
                self.prompt = None;
                self.control_mutation_accepted();
                return Ok(ControlAnswer::Done);
            }
            crate::control::DialogAnswer::Path(path) => path.clone(),
            _ => return Err(crate::Error::Unavailable),
        };
        let id = match prompt.action {
            PathAction::Open => self.control_open_job(path)?,
            PathAction::Dictionary => self.control_dictionary_job(path)?,
            PathAction::Save { .. } => self.control_save_job(target, Some(path))?,
        };
        self.prompt = None;
        Ok(ControlAnswer::Job(id))
    }

    fn control_conflict_answer(
        &mut self,
        dialog: u64,
        target: Target,
        answer: &crate::control::DialogAnswer,
    ) -> crate::Result<ControlAnswer> {
        if self.reloading.is_none() && self.conflict.is_none() {
            return Err(crate::Error::Unavailable);
        }
        if dialog == 0 || dialog != self.last_dialog_id {
            return Err(crate::Error::InvalidArgument);
        }
        let current = if let Some(current) = self.reloading {
            self.ui
                .editor()
                .revision_point(current.tab, current.revision)?;
            current
        } else {
            self.conflict
                .as_ref()
                .ok_or(crate::Error::Unavailable)?
                .target(self.ui.editor())?
        };
        if current.tab != target.tab {
            return Err(crate::Error::InvalidArgument);
        }
        if current.revision != target.revision {
            return Err(crate::Error::StaleRevision);
        }
        if matches!(answer, crate::control::DialogAnswer::Cancel) {
            self.cancel_conflict()?;
            self.control_mutation_accepted();
            return Ok(ControlAnswer::Done);
        }
        if self.reloading.is_some()
            || self.files.as_ref().is_some_and(|files| files.busy())
            || self.control_file_job.is_some()
            || self.prompt.is_some()
        {
            return Err(crate::Error::Unavailable);
        }
        let conflict = self.conflict.as_ref().ok_or(crate::Error::Unavailable)?;
        match answer {
            crate::control::DialogAnswer::SaveAs(path) if !conflict.needs_discard() => {
                let id = self.control_save_job(target, Some(path.clone()))?;
                self.conflict = None;
                return Ok(ControlAnswer::Job(id));
            }
            crate::control::DialogAnswer::Reload if !conflict.needs_discard() => {}
            crate::control::DialogAnswer::DiscardReload if conflict.needs_discard() => {}
            _ => return Err(crate::Error::Unavailable),
        }
        let discard = matches!(answer, crate::control::DialogAnswer::DiscardReload);
        if !discard && self.ui.editor().document(target.tab)?.dirty() {
            let conflict = self.conflict.as_mut().ok_or(crate::Error::Unavailable)?;
            if conflict.answer(self.ui.editor(), false)?.is_some() {
                return Err(crate::Error::Protocol);
            }
            self.control_mutation_accepted();
            return Ok(ControlAnswer::Dialog(dialog));
        }
        self.ui
            .generation()
            .checked_add(1)
            .ok_or(crate::Error::Exhausted)?;
        let id = self
            .control_jobs
            .begin_reload(target.tab, target.revision)?;
        let permit = self
            .conflict
            .as_mut()
            .ok_or(crate::Error::Unavailable)?
            .answer(self.ui.editor(), discard);
        match permit {
            Ok(Some(permit)) => {
                if self.start_reload(target, permit) {
                    self.control_file_job = Some(ControlFile::Reload(id));
                } else {
                    self.control_jobs
                        .reloaded(id, Err(crate::Error::Unavailable))?;
                }
            }
            other => {
                let code = other.err().unwrap_or(crate::Error::Protocol);
                self.control_jobs.reloaded(id, Err(code))?;
                self.conflict = None;
                self.notify(format!("Reload refused: {code}; text retained"));
            }
        }
        self.stop_pointer();
        self.control_mutation_accepted();
        Ok(ControlAnswer::Job(id))
    }

    fn control_close_save_job(
        &mut self,
        target: Target,
        path: Option<PathBuf>,
    ) -> crate::Result<ControlAnswer> {
        let id = self.control_save_job(target, path)?;
        self.prompt = None;
        self.closing_save = self.files.as_ref().is_some_and(|files| files.busy());
        if !self.closing_save {
            self.close_failed();
        }
        Ok(ControlAnswer::Job(id))
    }

    fn control_dialog_fields(&self) -> String {
        let mut fields = format!("dialog-last={}\tdialog=", self.last_dialog_id);
        let Some(close) = self.closing.as_ref() else {
            if let Some(target) = self.reloading {
                if self
                    .ui
                    .editor()
                    .revision_point(target.tab, target.revision)
                    .is_ok()
                {
                    fields.push_str(&format!(
                        "{},conflict,reloading,{},{},cancel",
                        self.last_dialog_id, target.tab, target.revision
                    ));
                } else {
                    fields.push_str(&format!("{},conflict,invalid,-,-,-", self.last_dialog_id));
                }
            } else if let Some(conflict) = &self.conflict {
                match conflict.target(self.ui.editor()) {
                    Ok(target) => {
                        let (phase, answers) = if conflict.needs_discard() {
                            ("discard", "cancel+discard-reload")
                        } else {
                            ("question", "cancel+reload+save-as")
                        };
                        fields.push_str(&format!(
                            "{},conflict,{phase},{},{},{answers}",
                            self.last_dialog_id, target.tab, target.revision
                        ));
                    }
                    Err(_) => {
                        fields.push_str(&format!("{},conflict,invalid,-,-,-", self.last_dialog_id))
                    }
                }
            } else if let Some(prompt) = &self.prompt {
                if let Some(identity) = &prompt.identity {
                    let scope = match prompt.action {
                        PathAction::Open => "path-open",
                        PathAction::Dictionary => "path-dictionary",
                        PathAction::Save { .. } => "path-save-as",
                    };
                    if self.ui.editor().check_revision(&identity.point).is_ok() {
                        fields.push_str(&format!(
                            "{},{scope},path,{},{},cancel+path",
                            identity.id, identity.point.tab, identity.point.revision
                        ));
                    } else {
                        fields.push_str(&format!("{},{scope},invalid,-,-,-", identity.id));
                    }
                } else {
                    fields.push('-');
                }
            } else {
                fields.push('-');
            }
            return fields;
        };
        let scope = match close.scope() {
            Scope::Tab { .. } => "close-tab",
            Scope::Window => "close-window",
        };
        let (phase, answers) = if self.closing_save {
            ("saving", "cancel")
        } else if self.prompt.is_some() {
            ("path", "cancel+path")
        } else {
            ("question", "cancel+discard+save")
        };
        match close.next(self.ui.editor()) {
            Ok(Some(target)) => fields.push_str(&format!(
                "{},{scope},{phase},{},{},{answers}",
                self.last_dialog_id, target.tab, target.revision
            )),
            _ => fields.push_str(&format!("{},{scope},invalid,-,-,-", self.last_dialog_id)),
        }
        fields
    }

    fn control_mutation_accepted(&mut self) {
        self.input.cancel_repeat();
        // Controller admission already cancelled its own drag.
        self.clear_pointer_gesture();
        if self.clipboard.incoming.take().is_some() {
            self.notify("Paste cancelled: remote control command accepted.");
        }
        self.clipboard.incoming_target = None;
        self.searches.observe(self.ui.editor());
        self.spelling.observe(self.ui.editor());
        self.observe_control_jobs();
        self.frames.invalidate(true);
    }

    fn native_state(&self, mut response: String) -> crate::Result<String> {
        if response.split('\t').nth(2) == Some("ok") {
            let flag = u8::from;
            response.push_str(&format!(
                concat!(
                    "\tadapter=native-paths\tnative={},{},{},{}",
                    "\tmodal={},{},{},{},{},{},{},{},{}\tspelling={},{}",
                ),
                flag(self.configured),
                flag(self.files.is_some()),
                flag(self.files.as_ref().is_some_and(|files| files.busy())),
                flag(self.quitting),
                flag(self.prompt.is_some()),
                flag(self.closing.is_some()),
                flag(self.conflict.is_some()),
                flag(self.reloading.is_some()),
                flag(self.menu.is_some()),
                flag(self.search.is_some()),
                flag(self.number.is_some()),
                flag(self.command.is_some()),
                flag(self.replace.is_some()),
                self.spelling
                    .dictionary_entries()
                    .map_or_else(|| "-".into(), |n| n.to_string()),
                flag(self.spelling.running()),
            ));
            response.push('\t');
            response.push_str(&self.control_jobs.fields()?);
            response.push('\t');
            response.push_str(&self.control_dialog_fields());
            response.push('\t');
            response.push_str(&self.frames.fields()?);
        }
        Ok(response)
    }

    fn stop_control(&mut self) {
        self.frame_waiters.clear();
        if let Some(worker) = self.control.take() {
            if let Err(error) = worker.close() {
                self.control_cleanup_error = Some(error.to_string());
            }
        }
    }

    fn disable_control(&mut self, detail: &str) {
        self.stop_control();
        let suffix = self
            .control_cleanup_error
            .as_ref()
            .map_or_else(String::new, |error| format!("; shutdown: {error}"));
        self.notify(format!("Control disabled: {detail}{suffix}"));
    }

    fn finish_control(&mut self, result: Result<()>) -> Result<()> {
        self.stop_control();
        match self.control_cleanup_error.take() {
            Some(detail) => Err(match result {
                Ok(()) => format!("Control shutdown failed: {detail}"),
                Err(previous) => format!("{previous}; control shutdown failed: {detail}"),
            }),
            None => result,
        }
    }

    fn draw(&mut self) -> Result<()> {
        self.frames.generation().map_err(error)?;
        if self.closed
            || !self.frames.is_dirty()
            || !self.configured
            || !self.xrgb
            || self.callback.is_some()
        {
            return Ok(());
        }
        let geometry = self.ui.geometry();
        let stamp = self.frames.capture(&self.ui).map_err(error)?;
        let free = self
            .buffers
            .iter()
            .position(|b| !b.busy && b.geometry == geometry)
            .or_else(|| self.buffers.iter().position(|b| !b.busy));
        if free.is_none() && self.buffers.len() == 3 {
            return Ok(());
        }
        let index = if let Some(index) = free {
            if self.buffers.get(index).ok_or("buffer slot")?.geometry != geometry {
                let old = self.buffers.remove(index);
                self.connection.words(old.id, 0, &[])?;
                self.set_kind(old.id, Kind::RetiredBuffer)?;
                self.create_buffer(geometry)?
            } else {
                index
            }
        } else {
            self.create_buffer(geometry)?
        };
        let (width, height) = geometry.dimensions();
        let size = width * height * 4;
        self.pixels.resize(size, 0);
        let labels: Vec<_> = self
            .labels
            .iter()
            .map(|(tab, title)| Label { tab: *tab, title })
            .chain(
                self.files
                    .iter()
                    .flat_map(|files| files.labels())
                    .map(|(tab, title)| Label { tab, title }),
            )
            .collect();
        let close_notice = self.close_notice();
        let path_notice = self.path_notice();
        let search_notice = self.search_notice();
        let number_notice = self.number_notice();
        let command_notice = self.command_notice();
        let replace_notice = self.replace_notice();
        let closing_notice = self.closing_notice();
        let conflict_notice = self.conflict_notice();
        let mut raster =
            Raster::new(&mut self.pixels, &self.font, geometry, width * 4).map_err(error)?;
        raster
            .paint(
                &self
                    .ui
                    .scene(&labels)
                    .map_err(error)?
                    .spelling(&self.spelling),
                geometry.bounds(),
            )
            .map_err(error)?;
        let notice = if self.quitting {
            Some(close_notice)
        } else if path_notice.is_some() {
            path_notice.as_deref()
        } else if closing_notice.is_some() {
            closing_notice.as_deref()
        } else if conflict_notice.is_some() {
            conflict_notice.as_deref()
        } else if number_notice.is_some() {
            number_notice.as_deref()
        } else if command_notice.is_some() {
            command_notice.as_deref()
        } else if search_notice.is_some() {
            search_notice.as_deref()
        } else if replace_notice.is_some() {
            replace_notice.as_deref()
        } else {
            self.notice.as_deref()
        };
        if let Some(notice) = notice {
            paint_notice(&mut raster, geometry, notice);
        }
        if let Some(menu) = &self.menu {
            menu.paint(&mut raster, geometry);
        }
        let buffer = self.buffers.get(index).ok_or("buffer slot")?;
        buffer.file.write_all_at(&self.pixels, 0).map_err(error)?;
        let id = buffer.id;
        let callback = self.allocate(Kind::Frame)?;
        self.connection
            .words(XDG_SURFACE, 3, &[0, 0, width as u32, height as u32])?;
        self.connection.words(SURFACE, 1, &[id, 0, 0])?;
        self.connection
            .words(SURFACE, 9, &[0, 0, width as u32, height as u32])?;
        self.connection.words(SURFACE, 3, &[callback])?;
        self.connection.words(SURFACE, 6, &[])?;
        self.buffers.get_mut(index).ok_or("buffer slot")?.busy = true;
        // Occluded surfaces may receive no callback until visible. Only the
        // initial handshake/submission has a deadline.
        self.connection.startup_deadline = None;
        self.callback = Some(callback);
        self.frames.submit(stamp).map_err(error)?;
        Ok(())
    }

    fn create_buffer(&mut self, geometry: Geometry) -> Result<usize> {
        let (width, height) = geometry.dimensions();
        let size = width * height * 4;
        let file = backing_file(&self.temporary, size)?;
        let pool = self.allocate(Kind::Pool)?;
        let buffer = self.allocate(Kind::Buffer)?;
        let mut body = Builder::new();
        body.u32(pool);
        body.u32(size as u32);
        self.connection.send(SHM, 0, body, Some(&file))?;
        self.connection.words(
            pool,
            0,
            &[
                buffer,
                0,
                width as u32,
                height as u32,
                (width * 4) as u32,
                1,
            ],
        )?;
        self.connection.words(pool, 1, &[])?;
        self.set_kind(pool, Kind::Retired)?;
        let index = self.buffers.len();
        self.buffers.push(Buffer {
            id: buffer,
            file,
            geometry,
            busy: false,
        });
        Ok(index)
    }

    fn run(&mut self) -> Result<()> {
        let started = Instant::now();
        let now = || u64::try_from(started.elapsed().as_millis()).map_err(error);
        let mut waiting: Option<(Message, Instant)> = None;
        self.connection.startup_deadline = Some(Instant::now() + INITIAL_DEADLINE);
        self.connection.words(DISPLAY, 1, &[REGISTRY])?;
        self.connection.words(DISPLAY, 0, &[SYNC])?;
        while !self.closed {
            self.connection.budget(WRITE_DEADLINE)?;
            let mut processed = 0;
            while processed < 256 {
                let next = match waiting.take() {
                    Some((message, deadline)) => {
                        if Instant::now() >= deadline {
                            return Err("Wayland descriptor deadline".into());
                        }
                        Some((message, deadline))
                    }
                    None => wire::take(&mut self.connection.pending)?
                        .map(|m| (m, Instant::now() + WRITE_DEADLINE)),
                };
                let Some((message, deadline)) = next else {
                    break;
                };
                if self.message_needs_descriptor(&message)?
                    && self.connection.descriptors.is_empty()
                {
                    if Instant::now() >= deadline {
                        return Err("Wayland descriptor deadline".into());
                    }
                    self.input.cancel_repeat();
                    waiting = Some((message, deadline));
                    break;
                }
                self.tick(now()?, false)?;
                self.event(message)?;
                processed += 1;
                if self.closed {
                    break;
                }
            }
            // Process queued releases/focus changes before a repeat can fire.
            self.end_turn(now()?, processed < 256 && waiting.is_none())?;
            self.draw()?;
            if !self.closed && processed < 256 {
                if let Some((_, deadline)) = &waiting {
                    self.connection.wait = self.connection.wait.min(
                        deadline
                            .checked_duration_since(Instant::now())
                            .filter(|d| !d.is_zero())
                            .ok_or("Wayland descriptor deadline")?,
                    );
                }
                self.connection.read_more()?;
            }
        }
        Ok(())
    }
}

impl Window {
    fn open_menu(&mut self, group: crate::menu::Group) -> Result<()> {
        if self.pointer_modal() {
            return Ok(());
        }
        let Some(tab) = self.ui.editor().active() else {
            return Ok(());
        };
        let doc = self.ui.editor().document(tab).map_err(error)?;
        let (undo, redo) = doc.history_depth();
        let mut menu = crate::menu::Menu {
            group,
            selected: 0,
            target: Target {
                tab,
                revision: doc.revision(),
            },
            profile: self.ui.keys().profile(),
            file_window: self.files.is_some(),
            undo: undo != 0,
            redo: redo != 0,
            auto_fill: doc.auto_fill(),
            wrap: self.ui.tab_view(tab).map_err(error)?.soft_wrap,
            copy: self.input.focused
                && self.clipboard.device.is_some()
                && self.clipboard.outgoing.is_none()
                && !doc.selection().range().is_empty(),
            paste: self.input.focused
                && self.clipboard.device.is_some()
                && self.clipboard.incoming.is_none()
                && self
                    .clipboard
                    .selection
                    .and_then(|id| self.clipboard.offers.get(&id).and_then(|o| o.mime()))
                    .is_some(),
        };
        if menu.panel(self.ui.geometry()).is_none() {
            self.menu = None;
            self.notify(
                "Enlarge the window to show the complete menu. Keyboard commands remain available.",
            );
            return Ok(());
        }
        if let Err(e) = self.ui.dispatch(Event::CancelInput) {
            self.notify(format!("Menu refused: {e}"));
            return Ok(());
        }
        self.stop_pointer();
        self.input.cancel_repeat();
        if menu
            .group
            .items()
            .first()
            .is_some_and(|item| !menu.enabled(*item))
        {
            menu.step(false);
        }
        self.menu = Some(menu);
        self.frames.invalidate(true);
        Ok(())
    }

    fn menu_valid(&self) -> bool {
        self.menu.as_ref().is_some_and(|menu| {
            self.ui.editor().active() == Some(menu.target.tab)
                && self.ui.keys().profile() == menu.profile
                && self
                    .ui
                    .editor()
                    .document(menu.target.tab)
                    .is_ok_and(|doc| doc.revision() == menu.target.revision)
                && menu.panel(self.ui.geometry()).is_some()
        })
    }

    fn menu_pointer(&mut self, x: i64, y: i64) -> Result<bool> {
        if let Some(group) = crate::menu::header(self.ui.geometry(), x, y) {
            if self.menu.as_ref().is_some_and(|menu| menu.group == group) {
                self.menu = None;
                self.frames.invalidate(true);
            } else {
                self.open_menu(group)?;
            }
            return Ok(true);
        }
        let Some(menu) = &self.menu else {
            return Ok(false);
        };
        if let Some(index) = menu.hit(self.ui.geometry(), x, y) {
            self.activate_menu(index)?;
        } else {
            self.menu = None;
            self.frames.invalidate(true);
        }
        Ok(true)
    }

    fn menu_chord(&mut self, chord: &str, repeated: bool) -> Result<()> {
        if repeated {
            return Ok(());
        }
        if matches!(chord, "Escape" | "C-g" | "F10") {
            self.menu = None;
            if chord != "F10" {
                self.notice = None;
            }
            self.frames.invalidate(true);
            return Ok(());
        }
        if !self.menu_valid() {
            self.menu = None;
            self.notify("Menu cancelled: document changed. Open the menu again.");
            return Ok(());
        }
        let Some(menu) = &mut self.menu else {
            return Ok(());
        };
        match chord {
            "Up" | "Down" => {
                menu.step(chord == "Up");
                self.frames.invalidate(true);
            }
            "Left" | "Right" => {
                let count = crate::menu::Group::ALL.len();
                let index =
                    (menu.group.index() + if chord == "Left" { count - 1 } else { 1 }) % count;
                let group = crate::menu::Group::ALL
                    .get(index)
                    .copied()
                    .ok_or("menu group")?;
                self.open_menu(group)?;
            }
            "Return" | "Space" => {
                let index = menu.selected;
                self.activate_menu(index)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn activate_menu(&mut self, index: usize) -> Result<()> {
        if !self.menu_valid() {
            self.menu = None;
            self.notify("Menu cancelled: document changed. Open the menu again.");
            return Ok(());
        }
        let Some(menu) = &self.menu else {
            return Ok(());
        };
        let Some(item) = menu.group.items().get(index).copied() else {
            return Ok(());
        };
        if !menu.enabled(item) {
            return Ok(());
        }
        let Target { tab, revision } = menu.target;
        let wrap = menu.wrap;
        let auto_fill = menu.auto_fill;
        self.activate_item(item, Target { tab, revision }, wrap, auto_fill)
    }

    fn activate_item(
        &mut self,
        item: crate::menu::Item,
        Target { tab, revision }: Target,
        wrap: bool,
        auto_fill: bool,
    ) -> Result<()> {
        use crate::menu::Item;
        self.menu = None;
        self.stop_pointer();
        self.input.cancel_repeat();
        self.frames.invalidate(true);
        let event = match item {
            Item::Command => {
                self.command_request(tab, revision)?;
                return Ok(());
            }
            Item::New => Event::New,
            Item::Open | Item::Save | Item::SaveAs => {
                self.file_request(
                    match item {
                        Item::Open => "open",
                        Item::Save => "save",
                        _ => "save-as",
                    },
                    tab,
                    revision,
                );
                return Ok(());
            }
            Item::Close => {
                self.close_tab(tab, revision);
                return Ok(());
            }
            Item::Quit => {
                self.close();
                return Ok(());
            }
            Item::Cut | Item::Copy | Item::Paste => {
                self.clipboard_request(
                    match item {
                        Item::Cut => "cut",
                        Item::Copy => "copy",
                        _ => "paste",
                    },
                    tab,
                    revision,
                )?;
                return Ok(());
            }
            Item::Undo => Event::Edit {
                tab,
                revision,
                command: crate::model::Command::Undo,
            },
            Item::Redo => Event::Edit {
                tab,
                revision,
                command: crate::model::Command::Redo,
            },
            Item::SelectAll => Event::Edit {
                tab,
                revision,
                command: crate::model::Command::Select(crate::model::Selection {
                    anchor: 0,
                    caret: self.ui.editor().document(tab).map_err(error)?.text().len(),
                }),
            },
            Item::Windows => Event::Profile(Profile::Windows),
            Item::Emacs => Event::Profile(Profile::Emacs),
            Item::Wrap => Event::Wrap {
                tab,
                revision,
                enabled: !wrap,
            },
            Item::AutoFill => Event::Edit {
                tab,
                revision,
                command: crate::model::Command::AutoFill(!auto_fill),
            },
            Item::Fill => Event::Edit {
                tab,
                revision,
                command: crate::model::Command::FillParagraph,
            },
            Item::About => {
                self.notify("td-editor: experimental Wayland text editor. Pure std Rust; bitmap Unifont, warm palette. UTF-8 clipboard needs data-device v3. F7 checks spelling with an explicit local word list. No recovery. Do not use as $EDITOR. F10 opens menus.");
                return Ok(());
            }
            Item::Spell => {
                // The shared action already renders startup feedback.
                let _ = self.spelling_request(tab, revision);
                return Ok(());
            }
            Item::Dictionary => {
                self.file_request("dictionary", tab, revision);
                return Ok(());
            }
            Item::NextMisspelling | Item::PreviousMisspelling => {
                self.spelling_selection(item == Item::PreviousMisspelling)?;
                return Ok(());
            }
            Item::GoToLine => {
                self.number_request(tab, revision, crate::number::Kind::Line)?;
                return Ok(());
            }
            Item::Replace => {
                self.replace_request(tab, revision)?;
                return Ok(());
            }
            Item::FillColumn => {
                self.number_request(tab, revision, crate::number::Kind::FillColumn)?;
                return Ok(());
            }
            Item::Find | Item::FindNext | Item::FindPrevious => {
                self.search_request(
                    match item {
                        Item::Find => "find",
                        Item::FindNext => "find-next",
                        _ => "find-previous",
                    },
                    tab,
                    revision,
                )?;
                return Ok(());
            }
        };
        if let Err(detail) = self.ui.dispatch(event) {
            self.notify(format!("Menu command refused: {detail}"));
        }
        Ok(())
    }

    fn close_tab(&mut self, tab: crate::model::TabId, revision: u64) {
        if self.files.is_some() {
            let _ = self.start_close(Scope::Tab { tab, revision });
            return;
        }
        match self.ui.dispatch(Event::Close { tab, revision }) {
            Ok(_) => {
                self.labels.retain(|(id, _)| *id != tab);
                self.closed |= self.ui.editor().active().is_none();
                self.frames.invalidate(true);
            }
            Err(crate::Error::Dirty) => self.notify(
                "Tab has unsaved scratch text. Undo to clean, or close the window to discard all.",
            ),
            Err(detail) => self.notify(detail.to_string()),
        }
    }

    fn start_close(&mut self, scope: Scope) -> crate::Result<()> {
        self.menu = None;
        self.stop_pointer();
        self.input.cancel_repeat();
        if self.closing.is_some() {
            return Ok(());
        }
        if self.reloading.is_some() || self.files.as_ref().is_some_and(|files| files.busy()) {
            self.notify(match scope {
                Scope::Tab { .. } => "File operation pending; wait before closing tabs.",
                Scope::Window => "File operation pending. Wait for completion, then close again; quitting does not cancel a write.",
            });
            return Err(crate::Error::Unavailable);
        }
        let prepared = (|| {
            let id = self
                .last_dialog_id
                .checked_add(1)
                .ok_or(crate::Error::Exhausted)?;
            Ok((id, Close::new(self.ui.editor(), scope)?))
        })();
        match prepared {
            Ok((id, close)) => {
                self.prompt = None;
                self.conflict = None;
                self.notice = None;
                self.closing = Some(close);
                self.last_dialog_id = id;
                self.advance_close()
            }
            Err(detail) => {
                self.notify(format!("Close refused: {detail}"));
                Err(detail)
            }
        }
    }

    fn advance_close(&mut self) -> crate::Result<()> {
        if self.files.as_ref().is_some_and(|files| files.busy()) {
            return Ok(());
        }
        self.frames.invalidate(true);
        let Some(close) = self.closing.as_ref() else {
            return Ok(());
        };
        match close.next(self.ui.editor()) {
            Ok(Some(_)) => return Ok(()),
            Err(detail) => {
                self.closing = None;
                self.notify(format!(
                    "Close cancelled: documents changed ({detail}); all remaining tabs retained."
                ));
                return Err(detail);
            }
            Ok(None) => {}
        }
        let Some(close) = self.closing.take() else {
            return Ok(());
        };
        match close.complete(&mut self.ui) {
            Ok(Closed::Window) => self.closed = true,
            Ok(Closed::Tab(tab)) => {
                self.labels.retain(|(id, _)| *id != tab);
                if let Some(files) = &mut self.files {
                    files.forget(tab);
                }
                self.closed = self.ui.editor().active().is_none();
            }
            Err(detail) => {
                self.notify(format!("Close cancelled: {detail}"));
                return Err(detail);
            }
        }
        Ok(())
    }

    fn cancel_close(&mut self) {
        self.closing = None;
        self.closing_save = false;
        self.prompt = None;
        self.notify(if self.files.as_ref().is_some_and(|f| f.busy()) {
            "Close cancelled; the pending Save will still finish."
        } else {
            "Close cancelled; tabs retained. Completed saves are not reverted."
        });
    }

    fn discard_close(&mut self, target: Target) -> crate::Result<()> {
        self.closing
            .as_mut()
            .ok_or(crate::Error::Unavailable)?
            .discard(self.ui.editor(), target)?;
        self.advance_close()
    }

    fn close_chord(&mut self, chord: &str, repeated: bool) {
        if repeated {
            return;
        }
        if matches!(chord, "Escape" | "C-g") {
            self.cancel_close();
            return;
        }
        if self.closing_save || !self.close_answer_visible() {
            return;
        }
        let Some(close) = &mut self.closing else {
            return;
        };
        let target = match close.next(self.ui.editor()) {
            Ok(Some(target)) => target,
            _ => {
                let _ = self.advance_close();
                return;
            }
        };
        match chord {
            "C-d" => {
                if let Err(detail) = self.discard_close(target) {
                    // Completion already consumes the coordinator and reports
                    // its own error. Only an approval failure needs cleanup.
                    if self.closing.take().is_some() {
                        self.notify(format!("Discard refused: {detail}"));
                    }
                }
            }
            "C-s" => {
                if !self.file_request("save", target.tab, target.revision) {
                    self.close_failed();
                } else {
                    self.closing_save = self.files.as_ref().is_some_and(|f| f.busy());
                }
            }
            _ => {}
        }
    }

    fn close_failed(&mut self) {
        self.closing = None;
        let detail = self.notice.take().unwrap_or_default();
        self.notify(format!("Close cancelled; tabs retained. {detail}"));
    }

    fn conflict_chord(&mut self, chord: &str, repeated: bool) {
        if repeated {
            return;
        }
        if matches!(chord, "Escape" | "C-g") {
            if let Err(detail) = self.cancel_conflict() {
                self.notify(format!("Reload cancellation outcome unavailable: {detail}"));
            }
            return;
        }
        if self.reloading.is_some() || !self.close_answer_visible() {
            return;
        }
        let Some(conflict) = self.conflict.as_mut() else {
            return;
        };
        let target = match conflict.target(self.ui.editor()) {
            Ok(target) => target,
            Err(detail) => {
                self.conflict = None;
                self.notify(format!(
                    "Conflict question cancelled: {detail}; text retained. Retry Save."
                ));
                return;
            }
        };
        if chord == "C-s" && !conflict.needs_discard() {
            self.conflict = None;
            self.file_request("save-as", target.tab, target.revision);
            return;
        }
        let discard = match chord {
            "C-r" => false,
            "C-d" if conflict.needs_discard() => true,
            _ => return,
        };
        match conflict.answer(self.ui.editor(), discard) {
            Ok(Some(permit)) => {
                self.start_reload(target, permit);
            }
            Ok(None) => self.frames.invalidate(true),
            Err(detail) => {
                self.conflict = None;
                self.notify(format!("Reload refused: {detail}; text retained"));
            }
        }
    }

    fn start_reload(&mut self, target: Target, permit: crate::Reload) -> bool {
        self.conflict = None;
        let result = self
            .files
            .as_mut()
            .ok_or_else(|| "File session unavailable".to_string())
            .and_then(|files| files.reload(&self.ui, permit));
        match result {
            Ok(()) => {
                self.reloading = Some(target);
                self.frames.invalidate(true);
                true
            }
            Err(detail) => {
                self.notify(format!("Reload refused: {detail}; text retained"));
                false
            }
        }
    }

    fn cancel_conflict(&mut self) -> crate::Result<()> {
        self.conflict = None;
        let outcome = if let Some(ControlFile::Reload(id)) = self.control_file_job.as_ref() {
            let result = self
                .control_jobs
                .reloaded(*id, Ok(ReloadOutcome::Cancelled));
            self.control_file_job = None;
            result
        } else {
            Ok(())
        };
        if self.reloading.take().is_some() {
            if let Some(files) = self.files.as_mut() {
                files.cancel_reload();
            }
            self.notify(
                "Reload cancelled; pending read will finish without replacing text or baseline.",
            );
        } else {
            // Keep the original file diagnostic available after dismissal.
            self.frames.invalidate(true);
        }
        outcome
    }

    fn conflict_notice(&self) -> Option<String> {
        if let Some(target) = self.reloading {
            return Some(format!("Tab {}\nReading Reload candidate...\nEscape/Ctrl+G cancels Reload.\nOld text kept until accepted.\nWait before closing the window.", target.tab));
        }
        let conflict = self.conflict.as_ref()?;
        let target = match conflict.target(self.ui.editor()) {
            Ok(target) => target,
            Err(detail) => {
                return Some(format!(
                    "Conflict question changed.\n{detail}\nEscape/Ctrl+G cancels.\nRetry Save."
                ))
            }
        };
        let identity = format!("Tab {}", target.tab);
        if self.device.is_none() || self.input.map.is_none() {
            return Some(format!(
                "{identity}\nInput unavailable.\nRestore the seat/keymap."
            ));
        }
        if !self.input.focused || !self.input.synchronized {
            return Some(format!("{identity}\nFocus editor.\nTap and release Shift."));
        }
        if !self.close_answer_visible() {
            return Some(format!(
                "{identity}\nEnlarge window to answer.\nEscape/Ctrl+G cancels."
            ));
        }
        let title = self
            .files
            .as_ref()
            .and_then(|files| {
                files
                    .labels()
                    .find(|(tab, _)| *tab == target.tab)
                    .map(|(_, title)| title)
            })
            .unwrap_or("Untitled");
        let short: String = title.chars().take(27).collect();
        let suffix = if short.len() < title.len() { "..." } else { "" };
        if conflict.needs_discard() {
            Some(format!("{identity}\n{short}{suffix}\nReload discards unsaved text.\nUndo history will be cleared.\nCtrl+D: Discard and reload\nEsc/Ctrl+G: Cancel entire Reload"))
        } else {
            let failure = if self
                .notice
                .as_deref()
                .is_some_and(|notice| notice.starts_with("Close cancelled;"))
            {
                "Close cancelled; Save failed."
            } else {
                "Disk conflict; Save failed."
            };
            Some(format!("{identity}\n{short}{suffix}\n{failure}\nCtrl+R: Reload\nCtrl+S: Save As (new path)\nEsc/Ctrl+G: Cancel, show error"))
        }
    }

    fn close_answer_visible(&self) -> bool {
        let (width, height) = self.ui.geometry().dimensions();
        width >= 272
            && height >= 160
            && self.device.is_some()
            && self.input.map.is_some()
            && self.input.focused
            && self.input.synchronized
    }

    fn closing_notice(&self) -> Option<String> {
        let close = self.closing.as_ref()?;
        let readiness = if self.device.is_none() || self.input.map.is_none() {
            "Input unavailable: restore the seat/keymap to resolve close.\n"
        } else if !self.input.focused || !self.input.synchronized {
            "Input pending: focus the editor; tap and release Shift.\n"
        } else {
            ""
        };
        if self.closing_save {
            return Some(format!("{readiness}Saving before close...\nEscape/Ctrl+G: Cancel closing (the Save still finishes)\nOther commands wait; no pending write is discarded."));
        }
        match close.next(self.ui.editor()) {
            Ok(Some(target)) => {
                let title = self.files.as_ref().and_then(|f| f.labels().find(|(tab, _)| *tab == target.tab).map(|(_, label)| label)).unwrap_or("Untitled");
                let identity = format!("Tab {}", target.tab);
                if !readiness.is_empty() {
                    return Some(format!("{identity}\n{readiness}"));
                }
                if !self.close_answer_visible() {
                    return Some(format!("{identity}\nEnlarge window to answer close.\nEscape/Ctrl+G cancels closing."));
                }
                let short: String = title.chars().take(27).collect();
                let suffix = if short.len() < title.len() { "..." } else { "" };
                Some(format!("{identity}\n{short}{suffix}\nCtrl+S: Save\nCtrl+D: Discard this tab\nEsc/Ctrl+G: Cancel closing\ncompleted saves stay saved"))
            }
            Ok(None) => None,
            Err(detail) => Some(format!("Close request changed ({detail}); Escape/Ctrl+G cancels.\nNo unapproved edits will be discarded.")),
        }
    }

    fn path_notice(&self) -> Option<String> {
        let prompt = self.prompt.as_ref()?;
        let readiness = if self.device.is_none() || self.input.map.is_none() {
            "Path entry paused: keyboard unavailable; restore the seat/keymap.\n"
        } else if !self.input.focused || !self.input.synchronized {
            "Path entry paused: focus the editor; tap and release Shift.\n"
        } else {
            ""
        };
        let closing = if self.closing.is_some() {
            "Save before close: Escape/Ctrl+G cancels ALL closing.\n"
        } else {
            ""
        };
        Some(format!("{readiness}{closing}{}", prompt.notice()))
    }

    fn spelling_request(
        &mut self,
        tab: crate::model::TabId,
        revision: u64,
    ) -> crate::Result<Option<u64>> {
        self.menu = None;
        self.stop_pointer();
        self.input.cancel_repeat();
        self.clipboard.incoming = None;
        self.searches.cancel_wrap();
        if let Err(detail) = self.ui.dispatch(Event::CancelInput) {
            self.notify(format!("Spelling request refused: {detail}"));
            return Err(detail);
        }
        self.frames.invalidate(true);
        let result = self.spelling.start(self.ui.editor(), tab, revision);
        match result {
            Ok(Some(_)) => self.notice = None,
            Ok(None) => self.notify(if self.files.is_some() {
                "Spelling: no dictionary. Use Format > Dictionary to load a local English word list."
            } else {
                "Spelling: no dictionary. Scratch preview cannot load dictionaries; use --window."
            }),
            Err(detail) => self.notify(format!("Spelling request refused: {detail}")),
        }
        self.observe_control_jobs();
        result
    }

    fn observe_control_jobs(&mut self) {
        self.frames
            .invalidate(self.control_jobs.observe(self.ui.editor(), &self.spelling));
    }

    fn spelling_selection(&mut self, previous: bool) -> Result<()> {
        if self.spelling.checking_active(self.ui.editor()) {
            self.notify("Spelling is still checking this document. Wait for results, or Escape/Ctrl+G to cancel.");
            return Ok(());
        }
        let Some(range) = self
            .spelling
            .select(self.ui.editor(), previous)
            .map_err(error)?
        else {
            self.notify("No stored misspelling in that direction. Check Spelling with F7; navigation does not wrap.");
            return Ok(());
        };
        let tab = self.ui.editor().active().ok_or("no document")?;
        let revision = self.ui.editor().document(tab).map_err(error)?.revision();
        self.ui
            .dispatch(Event::Edit {
                tab,
                revision,
                command: crate::model::Command::Select(crate::model::Selection {
                    anchor: range.start,
                    caret: range.end,
                }),
            })
            .map_err(error)?;
        self.frames.invalidate(true);
        self.cancel_stale_paste();
        Ok(())
    }

    fn file_request(&mut self, name: &str, tab: crate::model::TabId, revision: u64) -> bool {
        self.menu = None;
        self.stop_pointer();
        let Some(files) = &mut self.files else {
            return false;
        };
        self.input.cancel_repeat();
        if files.busy() {
            self.notify("File operation pending; wait before trying again.");
            false
        } else if name == "save" && files.associated(tab) {
            let result = files.save(&self.ui, tab, revision, None);
            let accepted = result.is_ok();
            self.notify(match result {
                Ok(()) => "Saving snapshot; newer edits will remain unsaved.".into(),
                Err(detail) => detail,
            });
            accepted
        } else {
            let identity = if self.closing.is_some() {
                None
            } else {
                let prepared = self
                    .last_dialog_id
                    .checked_add(1)
                    .ok_or(crate::Error::Exhausted)
                    .and_then(|id| {
                        self.ui
                            .editor()
                            .revision_point(tab, revision)
                            .map(|point| PathIdentity { id, point })
                    });
                match prepared {
                    Ok(identity) => {
                        self.last_dialog_id = identity.id;
                        Some(identity)
                    }
                    Err(detail) => {
                        self.notify(format!("Path question refused: {detail}; text retained"));
                        return false;
                    }
                }
            };
            self.notice = None;
            self.prompt = Some(PathPrompt {
                action: if name == "open" {
                    PathAction::Open
                } else if name == "dictionary" {
                    PathAction::Dictionary
                } else {
                    PathAction::Save { tab, revision }
                },
                text: String::new(),
                identity,
            });
            self.frames.invalidate(true);
            true
        }
    }

    fn path_chord(&mut self, chord: &str, repeated: bool) {
        if repeated {
            return;
        }
        let Some(mut prompt) = self.prompt.take() else {
            return;
        };
        self.frames.invalidate(true);
        match chord {
            "Escape" | "C-g" => {
                if self.closing.take().is_some() {
                    self.notify(
                        "Close cancelled; tabs retained. Completed saves are not reverted.",
                    );
                }
                self.closing_save = false;
                return;
            }
            "Return" => {
                if prompt.text.is_empty() {
                    self.prompt = Some(prompt);
                    return;
                }
                let Some(files) = &mut self.files else {
                    return;
                };
                let path = PathBuf::from(&prompt.text);
                let result = match prompt.action {
                    PathAction::Open => files.open(path),
                    PathAction::Dictionary => files.dictionary(path),
                    PathAction::Save { tab, revision } => {
                        files.save(&self.ui, tab, revision, Some(path))
                    }
                };
                let close_failed = self.closing.is_some() && result.is_err();
                if self.closing.is_some() {
                    self.closing_save = result.is_ok();
                }
                self.notify(match result {
                    Ok(()) => "File operation pending...".into(),
                    Err(detail) => detail,
                });
                if close_failed {
                    self.close_failed();
                }
                return;
            }
            "Backspace" => {
                prompt.text.pop();
            }
            "C-u" => prompt.text.clear(),
            _ => {
                let text = if chord == "Space" { " " } else { chord };
                let mut chars = text.chars();
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    if !c.is_control() && prompt.text.len() + c.len_utf8() <= 4096 {
                        prompt.text.push(c);
                    }
                }
            }
        }
        self.prompt = Some(prompt);
    }

    fn close_notice(&self) -> &'static str {
        if self.device.is_none() || self.input.map.is_none() {
            "Cannot confirm discard: keyboard unavailable.\nScratch text is retained in memory.\nRestore keyboard/map to cancel or discard.\nTerminating this process loses ALL scratch text."
        } else if !self.input.focused || !self.input.synchronized {
            "Discard pending; input not ready.\nFocus the editor; tap and release Shift.\nThen Ctrl+D: Discard ALL; Escape/Ctrl+G: Cancel.\nSave is unavailable in this scratch preview."
        } else {
            "Discard ALL unsaved scratch text?\nSave is unavailable in this preview.\nCtrl+D: Discard and quit\nEscape / Ctrl+G: Cancel"
        }
    }
}

impl Window {
    fn command_notice(&self) -> Option<String> {
        self.command.as_ref().map(|prompt| {
            let paused = if self.device.is_none() || self.input.map.is_none() {
                "Command paused: keyboard unavailable; restore the seat/keymap.\n"
            } else if !self.input.focused || !self.input.synchronized {
                "Command paused: focus the editor; tap and release Shift.\n"
            } else {
                ""
            };
            format!("{paused}{}", prompt.notice())
        })
    }

    fn replace_notice(&self) -> Option<String> {
        self.replace.as_ref().map(|prompt| {
            let paused = if self.device.is_none() || self.input.map.is_none() {
                Some("Replace paused: restore seat/keymap.")
            } else if !self.input.focused || !self.input.synchronized {
                Some("Replace paused: focus; tap Shift.")
            } else {
                None
            };
            prompt.notice(paused)
        })
    }

    fn replace_request(&mut self, tab: crate::model::TabId, revision: u64) -> Result<()> {
        let prompt =
            match crate::replace::Prompt::new(self.ui.editor(), tab, revision, &self.searches) {
                Ok(prompt) => prompt,
                Err(detail) => {
                    self.notify(format!("Replace refused: {detail}"));
                    return Ok(());
                }
            };
        self.ui.dispatch(Event::CancelInput).map_err(error)?;
        self.stop_pointer();
        self.input.cancel_repeat();
        self.clipboard.incoming = None;
        self.searches.cancel_wrap();
        self.menu = None;
        self.search = None;
        self.number = None;
        self.command = None;
        self.replace = Some(prompt);
        self.notice = None;
        self.frames.invalidate(true);
        Ok(())
    }

    fn replace_chord(&mut self, chord: &str, repeated: bool) -> Result<()> {
        if repeated {
            return Ok(());
        }
        let Some(mut prompt) = self.replace.take() else {
            return Ok(());
        };
        self.frames.invalidate(true);
        if matches!(chord, "Escape" | "C-g") {
            self.searches.cancel_wrap();
            self.notice = None;
            self.ui.dispatch(Event::CancelInput).map_err(error)?;
            return Ok(());
        }
        let action = match chord {
            "Return" => Some(crate::replace::Action::Find),
            "M-r" => Some(crate::replace::Action::One),
            "M-a" => Some(crate::replace::Action::All),
            _ => None,
        };
        if let Some(action) = action {
            if let Err(detail) = prompt.apply(&mut self.ui, &mut self.searches, action) {
                self.searches.cancel_wrap();
                self.notify(format!(
                    "Replace cancelled: target changed or action refused ({detail})."
                ));
                return Ok(());
            }
        } else {
            prompt.type_chord(chord, &mut self.searches);
        }
        self.replace = Some(prompt);
        Ok(())
    }

    fn command_request(&mut self, tab: crate::model::TabId, revision: u64) -> Result<()> {
        let prompt = match crate::command::Prompt::new(self.ui.editor(), tab, revision) {
            Ok(prompt) => prompt,
            Err(detail) => {
                self.notify(format!("Command prompt refused: {detail}"));
                return Ok(());
            }
        };
        self.ui.dispatch(Event::CancelInput).map_err(error)?;
        self.stop_pointer();
        self.input.cancel_repeat();
        self.clipboard.incoming = None;
        self.searches.cancel_wrap();
        self.menu = None;
        self.search = None;
        self.number = None;
        self.command = Some(prompt);
        self.replace = None;
        self.notice = None;
        self.frames.invalidate(true);
        Ok(())
    }

    fn command_chord(&mut self, chord: &str, repeated: bool) -> Result<()> {
        if repeated {
            return Ok(());
        }
        let Some(mut prompt) = self.command.take() else {
            return Ok(());
        };
        self.frames.invalidate(true);
        if matches!(chord, "Escape" | "C-g") {
            self.notice = None;
            self.ui.dispatch(Event::CancelInput).map_err(error)?;
            return Ok(());
        }
        if chord == "Return" {
            let (tab, revision) = match prompt.target(self.ui.editor()) {
                Ok(target) => target,
                Err(detail) => {
                    self.notify(format!(
                        "Command cancelled: document or selection changed ({detail})."
                    ));
                    return Ok(());
                }
            };
            if let Ok(item) = prompt.action() {
                let auto_fill = self.ui.editor().document(tab).map_err(error)?.auto_fill();
                let wrap = self.ui.tab_view(tab).map_err(error)?.soft_wrap;
                return self.activate_item(item, Target { tab, revision }, wrap, auto_fill);
            }
            prompt.refused();
        } else {
            prompt.type_chord(chord);
        }
        self.command = Some(prompt);
        Ok(())
    }

    fn number_notice(&self) -> Option<String> {
        self.number.as_ref().map(|prompt| {
            let paused = if self.device.is_none() || self.input.map.is_none() {
                "keyboard unavailable; restore the seat/keymap.\n"
            } else if !self.input.focused || !self.input.synchronized {
                "focus the editor; tap and release Shift.\n"
            } else {
                ""
            };
            if paused.is_empty() {
                prompt.notice()
            } else {
                format!(
                    "{} paused: {paused}{}",
                    prompt.kind().label(),
                    prompt.notice()
                )
            }
        })
    }

    fn number_request(
        &mut self,
        tab: crate::model::TabId,
        revision: u64,
        kind: crate::number::Kind,
    ) -> Result<()> {
        self.menu = None;
        self.stop_pointer();
        self.input.cancel_repeat();
        let prompt = match crate::number::Prompt::new(self.ui.editor(), tab, revision, kind) {
            Ok(prompt) => prompt,
            Err(e) => {
                self.notify(format!("{} refused: {e}", kind.label()));
                return Ok(());
            }
        };
        self.ui.dispatch(Event::CancelInput).map_err(error)?;
        self.searches.cancel_wrap();
        self.number = Some(prompt);
        self.search = None;
        self.command = None;
        self.replace = None;
        self.clipboard.incoming = None;
        self.notice = None;
        self.frames.invalidate(true);
        Ok(())
    }

    fn number_chord(&mut self, chord: &str, repeated: bool) -> Result<()> {
        if repeated {
            return Ok(());
        }
        let Some(mut prompt) = self.number.take() else {
            return Ok(());
        };
        self.frames.invalidate(true);
        if matches!(chord, "Escape" | "C-g") {
            self.notice = None;
            self.ui.dispatch(Event::CancelInput).map_err(error)?;
            return Ok(());
        }
        if chord == "Return" {
            let (tab, revision) = match prompt.target(self.ui.editor()) {
                Ok(target) => target,
                Err(e) => {
                    self.notify(format!(
                        "{} cancelled: {} ({e}).",
                        prompt.kind().label(),
                        prompt.kind().changed()
                    ));
                    return Ok(());
                }
            };
            let result = prompt.command().and_then(|command| {
                self.ui.dispatch(Event::Edit {
                    tab,
                    revision,
                    command,
                })
            });
            match result {
                Ok(_) => {
                    self.notify(prompt.kind().success());
                    return Ok(());
                }
                Err(crate::Error::InvalidArgument | crate::Error::InvalidPosition) => {
                    prompt.refused()
                }
                Err(e) => {
                    self.notify(format!("{} refused: {e}", prompt.kind().label()));
                    return Ok(());
                }
            }
        } else {
            prompt.type_chord(chord);
        }
        self.number = Some(prompt);
        Ok(())
    }

    fn search_notice(&self) -> Option<String> {
        self.search.as_ref().map(|prompt| {
            let paused = if self.device.is_none() || self.input.map.is_none() {
                "Find paused: keyboard unavailable; restore the seat/keymap.\n"
            } else if !self.input.focused || !self.input.synchronized {
                "Find paused: focus the editor; tap and release Shift.\n"
            } else {
                ""
            };
            format!("{paused}{}", prompt.notice())
        })
    }

    fn search_request(
        &mut self,
        name: &str,
        tab: crate::model::TabId,
        revision: u64,
    ) -> Result<()> {
        self.menu = None;
        self.stop_pointer();
        self.input.cancel_repeat();
        let backward = matches!(name, "find-backward" | "find-previous");
        if matches!(name, "find-next" | "find-previous") && !self.searches.query().is_empty() {
            let query = self.searches.query().to_owned();
            self.find(tab, revision, &query, backward);
            return Ok(());
        }
        let doc = self.ui.editor().document(tab).map_err(error)?;
        let selected = doc.text().get(doc.selection().range()).unwrap_or_default();
        let query = if !selected.is_empty()
            && selected.len() <= crate::search::QUERY_BYTES
            && !selected.chars().any(char::is_control)
        {
            selected.to_owned()
        } else {
            self.searches.query().to_owned()
        };
        let prompt =
            match crate::search::Prompt::new(self.ui.editor(), tab, revision, query, backward) {
                Ok(prompt) => prompt,
                Err(e) => {
                    self.notify(format!("Find refused: {e}"));
                    return Ok(());
                }
            };
        if let Err(e) = self.ui.dispatch(Event::CancelInput) {
            self.notify(format!("Find refused: {e}"));
            return Ok(());
        }
        self.search = Some(prompt);
        self.clipboard.incoming = None;
        self.command = None;
        self.replace = None;
        self.notice = None;
        self.frames.invalidate(true);
        Ok(())
    }

    fn find(&mut self, tab: crate::model::TabId, revision: u64, query: &str, backward: bool) {
        use crate::search::Found;
        let result = self
            .searches
            .find(&mut self.ui, tab, revision, query, backward);
        self.notify(match result {
            Ok(Found::Match) => "Match found.".into(),
            Ok(Found::Wrapped) => "Search wrapped; match found.".into(),
            Ok(Found::End) => format!(
                "Reached {}; repeat this search to wrap.",
                if backward { "start" } else { "end" }
            ),
            Ok(Found::Missing) => "No matches in this document.".into(),
            Err(e) => format!("Find refused: {e}"),
        });
    }

    fn search_chord(&mut self, chord: &str, repeated: bool) -> Result<()> {
        if repeated {
            return Ok(());
        }
        let Some(mut prompt) = self.search.take() else {
            return Ok(());
        };
        self.frames.invalidate(true);
        if matches!(chord, "Escape" | "C-g") {
            self.searches.cancel_wrap();
            self.notice = None;
            if let Err(e) = self.ui.dispatch(Event::CancelInput) {
                self.notify(format!("Find cancellation refused: {e}"));
            }
            return Ok(());
        }
        let emacs_search =
            self.ui.keys().profile() == Profile::Emacs && matches!(chord, "C-s" | "C-r");
        if chord == "Return" || emacs_search {
            if emacs_search {
                prompt.backward = chord == "C-r";
            }
            if prompt.text.is_empty() {
                self.search = Some(prompt);
                return Ok(());
            }
            match prompt.target(self.ui.editor()) {
                Ok((tab, revision)) => self.find(tab, revision, &prompt.text, prompt.backward),
                Err(e) => self.notify(format!(
                    "Find cancelled: document or selection changed ({e})."
                )),
            }
            return Ok(());
        }
        prompt.type_chord(chord);
        self.search = Some(prompt);
        Ok(())
    }

    fn initialize_clipboard(&mut self) -> Result<()> {
        let Some(seat) = self.seat else { return Ok(()) };
        let Some(name) = self
            .globals
            .iter()
            .find(|(_, (name, version))| name == "wl_data_device_manager" && *version >= 3)
            .map(|(id, _)| *id)
        else {
            return Ok(());
        };
        let manager = self.allocate(Kind::ClipboardManager)?;
        let device = self.allocate(Kind::ClipboardDevice)?;
        let mut body = Builder::new();
        body.u32(name);
        body.string("wl_data_device_manager")?;
        body.u32(3);
        body.u32(manager);
        self.connection.send(REGISTRY, 0, body, None)?;
        self.connection.words(manager, 1, &[device, seat])?;
        self.clipboard.manager = Some((name, manager));
        self.clipboard.device = Some(device);
        Ok(())
    }

    fn message_needs_descriptor(&self, message: &Message) -> Result<bool> {
        // Validate the complete schema before deciding to wait for or consume
        // a right. Server-created offer IDs do not index the client table.
        if self.clipboard.offers.contains_key(&message.object) {
            crate::data::offer(message)?;
            return Ok(false);
        }
        match (self.kind(message.object)?, message.opcode) {
            (Kind::Keyboard | Kind::RetiredKeyboard, 0) => {
                keyboard_event(message)?;
                Ok(true)
            }
            (Kind::ClipboardSource | Kind::RetiredClipboardSource, 1) => {
                crate::data::source(message)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn retire_offer(&mut self, id: u32) -> Result<()> {
        let offer = self
            .clipboard
            .offers
            .get_mut(&id)
            .ok_or("unknown clipboard offer")?;
        if offer.retired {
            return Ok(());
        }
        offer.retired = true;
        self.connection.words(id, 2, &[])?;
        self.queue_offer_barrier()
    }

    fn queue_offer_barrier(&mut self) -> Result<()> {
        if !self.clipboard.barriers.is_empty() {
            return Ok(());
        }
        let retired: Vec<_> = self
            .clipboard
            .offers
            .iter()
            .filter(|(_, offer)| offer.retired)
            .map(|(id, offer)| (*id, offer.sequence))
            .collect();
        if retired.is_empty() {
            return Ok(());
        }
        let barrier = self.allocate(Kind::ClipboardSync)?;
        self.clipboard.barriers.insert(barrier, retired);
        self.connection.words(DISPLAY, 0, &[barrier])
    }

    fn clipboard_focus_lost(&mut self) -> Result<()> {
        if self.clipboard.incoming.take().is_some() {
            self.notify("Paste cancelled: clipboard focus lost.");
        }
        self.clipboard.selection = None;
        let offers: Vec<_> = self.clipboard.offers.keys().copied().collect();
        for offer in offers {
            self.retire_offer(offer)?;
        }
        Ok(())
    }

    fn retire_source(&mut self, id: u32) -> Result<()> {
        self.connection.words(id, 1, &[])?;
        self.set_kind(id, Kind::RetiredClipboardSource)
    }

    fn release_clipboard(&mut self) -> Result<()> {
        self.clipboard_focus_lost()?;
        if let Some(transfer) = self.clipboard.outgoing.take() {
            if let Err(e) = transfer.cancel() {
                self.notify(format!("Clipboard close: {e}"));
            }
        }
        if let Some((source, _)) = self.clipboard.source.take() {
            self.retire_source(source)?;
        }
        if let Some(device) = self.clipboard.device.take() {
            self.connection.words(device, 2, &[])?;
            self.set_kind(device, Kind::RetiredClipboardDevice)?;
        }
        self.menu = None;
        self.frames.invalidate(true);
        Ok(())
    }

    fn clipboard_offer(&mut self, message: Message) -> Result<()> {
        let mime = crate::data::offer(&message)?;
        let offer = self
            .clipboard
            .offers
            .get_mut(&message.object)
            .ok_or("unknown clipboard offer")?;
        if let Some(mime) = mime {
            if offer.count >= 64 {
                return Ok(());
            }
            offer.count += 1;
            if mime.len() > 256 {
                return Ok(());
            }
            if !offer.retired {
                if mime.eq_ignore_ascii_case(crate::data::UTF8) && offer.utf8.is_none() {
                    offer.utf8 = Some(mime);
                } else if mime.eq_ignore_ascii_case(crate::data::PLAIN) && offer.plain.is_none() {
                    offer.plain = Some(mime);
                }
            }
        }
        Ok(())
    }

    fn clipboard_device(&mut self, message: Message) -> Result<()> {
        use crate::data::DeviceEvent as D;
        let event = crate::data::device(&message)?;
        let active = self.clipboard.device == Some(message.object);
        if let D::Offer(id) = event {
            if id < 0xff00_0000 || self.clipboard.offers.get(&id).is_some_and(|o| !o.retired) {
                return Err("invalid server clipboard offer ID".into());
            }
            if !self.clipboard.offers.contains_key(&id)
                && self.clipboard.offers.len() >= crate::data::OFFER_LIMIT
            {
                return Err("clipboard offer budget".into());
            }
            self.clipboard.sequence = self
                .clipboard
                .sequence
                .checked_add(1)
                .ok_or("clipboard offer sequence exhausted")?;
            self.clipboard.offers.insert(
                id,
                crate::data::Offer {
                    sequence: self.clipboard.sequence,
                    retired: false,
                    utf8: None,
                    plain: None,
                    count: 0,
                },
            );
            if !active {
                self.retire_offer(id)?;
            }
            return Ok(());
        }
        if !active {
            return Ok(());
        }
        match event {
            D::Selection(id) => {
                if id != 0 && !self.clipboard.offers.get(&id).is_some_and(|o| !o.retired) {
                    return Err("selection names unknown or retired offer".into());
                }
                if self.clipboard.incoming.take().is_some() {
                    self.notify("Paste cancelled: clipboard offer changed.");
                }
                self.clipboard.selection = (id != 0).then_some(id);
                let offers: Vec<_> = self
                    .clipboard
                    .offers
                    .keys()
                    .copied()
                    .filter(|id| Some(*id) != self.clipboard.selection)
                    .collect();
                for offer in offers {
                    self.retire_offer(offer)?;
                }
                self.menu = None;
                self.frames.invalidate(true);
            }
            D::Enter { surface, offer } => {
                if surface != SURFACE {
                    return Err("data-device enter for unknown surface".into());
                }
                // No drag-and-drop support: refuse the offer without accepting,
                // finishing or treating it as the clipboard selection.
                if offer != 0 {
                    if self.clipboard.selection == Some(offer) {
                        return Err("drag reused selection offer".into());
                    }
                    if self.clipboard.offers.contains_key(&offer) {
                        self.retire_offer(offer)?;
                    }
                }
            }
            D::Drag | D::Offer(_) => {}
        }
        Ok(())
    }

    fn clipboard_source(&mut self, message: Message) -> Result<()> {
        let event = crate::data::source(&message)?;
        let active = self
            .clipboard
            .source
            .as_ref()
            .is_some_and(|(id, _)| *id == message.object);
        match event {
            crate::data::SourceEvent::Send(mime) => {
                let fd = self
                    .connection
                    .descriptors
                    .pop_front()
                    .ok_or("missing clipboard destination")?;
                if active
                    && matches!(mime.as_str(), crate::data::UTF8 | crate::data::PLAIN)
                    && self.clipboard.outgoing.is_none()
                {
                    let text = self
                        .clipboard
                        .source
                        .as_ref()
                        .map(|(_, text)| text.clone())
                        .ok_or("clipboard source disappeared")?;
                    match crate::transfer::Outgoing::begin(fd, text, self.clock) {
                        Ok(transfer) => self.clipboard.outgoing = Some(transfer),
                        Err(e) => self.notify(format!("Clipboard send refused: {e}")),
                    }
                }
                // Unsupported, busy and retired sends drop exactly their fd.
            }
            crate::data::SourceEvent::Cancel if active => {
                self.clipboard.source = None;
                self.retire_source(message.object)?;
                // An already-started send retains its immutable snapshot.
            }
            _ => {}
        }
        Ok(())
    }

    fn clipboard_request(
        &mut self,
        name: &str,
        tab: crate::model::TabId,
        revision: u64,
    ) -> Result<()> {
        if !self.input.focused || self.pointer_modal() || self.clipboard.device.is_none() {
            self.notify("Clipboard unavailable: focus the window and require data-device v3.");
            return Ok(());
        }
        if name == "paste" {
            if self.clipboard.incoming.is_some() {
                self.notify("Paste already in progress; Escape cancels.");
                return Ok(());
            }
            let Some((offer, mime)) = self.clipboard.selection.and_then(|id| {
                self.clipboard
                    .offers
                    .get(&id)
                    .and_then(|offer| offer.mime())
                    .map(|mime| (id, mime))
            }) else {
                self.notify("Clipboard has no supported UTF-8 text offer.");
                return Ok(());
            };
            let (transfer, peer) =
                match crate::transfer::Incoming::begin(self.ui.editor(), tab, revision, self.clock)
                {
                    Ok(pair) => pair,
                    Err(e) => {
                        self.notify(format!("Paste refused: {e}"));
                        return Ok(());
                    }
                };
            let mut body = Builder::new();
            body.string(mime)?;
            self.connection.send(offer, 1, body, Some(&peer))?;
            drop(peer);
            self.clipboard.incoming = Some(transfer);
            self.clipboard.incoming_target = Some((
                tab,
                revision,
                self.ui.editor().document(tab).map_err(error)?.selection(),
            ));
            self.notify("Pasting UTF-8 text; Escape cancels.");
            return Ok(());
        }
        let Some(serial) = self.activation_serial else {
            self.notify("Copy/Cut requires a current physical key or pointer press.");
            return Ok(());
        };
        if self.clipboard.outgoing.is_some() {
            self.notify("Clipboard is being sent; retry Copy/Cut after it completes.");
            return Ok(());
        }
        let snapshot = match crate::clipboard::Snapshot::capture(self.ui.editor(), tab, revision) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => {
                self.notify("Nothing selected to copy.");
                return Ok(());
            }
            Err(e) => {
                self.notify(format!("Copy refused: {e}"));
                return Ok(());
            }
        };
        let (_, manager) = self.clipboard.manager.ok_or("missing clipboard manager")?;
        let device = self.clipboard.device.ok_or("missing clipboard device")?;
        let source = self.allocate(Kind::ClipboardSource)?;
        self.connection.words(manager, 0, &[source])?;
        for mime in [crate::data::UTF8, crate::data::PLAIN] {
            let mut body = Builder::new();
            body.string(mime)?;
            self.connection.send(source, 0, body, None)?;
        }
        self.connection.words(device, 1, &[source, serial])?;
        if let Some((old, _)) = self.clipboard.source.replace((source, snapshot.text())) {
            self.retire_source(old)?;
        }
        self.clipboard.incoming = None;
        if name == "cut" {
            match self.ui.dispatch(Event::Cut(snapshot)) {
                Ok(_) => self.notify("Cut offered to clipboard; Undo restores the selection."),
                Err(e) => self.notify(format!("Copied, but Cut refused: {e}")),
            }
        } else {
            self.notify("Selection offered to clipboard.");
        }
        Ok(())
    }

    fn clipboard_tick(&mut self, now: u64, admit: bool) -> Result<()> {
        self.cancel_stale_paste();
        let incoming = if self
            .clipboard
            .incoming
            .as_ref()
            .is_some_and(|transfer| admit || transfer.expired(now))
        {
            self.clipboard.incoming.take()
        } else {
            None
        };
        if let Some(mut transfer) = incoming {
            match transfer.step(self.ui.editor(), now) {
                Ok(false) => self.clipboard.incoming = Some(transfer),
                Ok(true) => {
                    self.menu = None;
                    match transfer
                        .finish()
                        .map_err(error)
                        .and_then(|paste| self.ui.dispatch(Event::Paste(paste)).map_err(error))
                    {
                        Ok(Outcome::Ignored) => {
                            self.notify("Clipboard text was empty; selection retained.")
                        }
                        Ok(_) => self.notify("Paste complete."),
                        Err(e) => self.notify(format!("Paste refused: {e}")),
                    }
                }
                Err(e) => self.notify(format!("Paste cancelled: {e}")),
            }
        }
        let outgoing = if self
            .clipboard
            .outgoing
            .as_ref()
            .is_some_and(|transfer| admit || transfer.expired(now))
        {
            self.clipboard.outgoing.take()
        } else {
            None
        };
        if let Some(mut transfer) = outgoing {
            match transfer.step(now) {
                Ok(false) => self.clipboard.outgoing = Some(transfer),
                Ok(true) => {}
                Err(e) => self.notify(format!("Clipboard send failed: {e}")),
            }
        }
        Ok(())
    }

    fn cancel_stale_paste(&mut self) {
        if self.clipboard.incoming.is_some()
            && (self.pointer_modal()
                || !self.input.focused
                || !self
                    .clipboard
                    .incoming_target
                    .is_some_and(|(tab, revision, selection)| {
                        self.ui.editor().active() == Some(tab)
                            && self.ui.editor().document(tab).is_ok_and(|doc| {
                                doc.revision() == revision && doc.selection() == selection
                            })
                    }))
        {
            self.clipboard.incoming = None;
            self.notify("Paste cancelled: focus, document or selection changed.");
        }
        if self.clipboard.incoming.is_none() {
            self.clipboard.incoming_target = None;
        }
    }
}

enum KeyboardEvent {
    Map(u32, u32),
    Enter(u32, Vec<u32>),
    Leave(u32),
    Key(u32, u32, bool),
    Modifiers(Modifiers),
    Timing(i32, i32),
}

fn keyboard_event(message: &Message) -> Result<KeyboardEvent> {
    let mut cursor = Cursor::new(&message.payload);
    let event = match message.opcode {
        0 => KeyboardEvent::Map(cursor.u32()?, cursor.u32()?),
        1 => {
            cursor.u32()?;
            let surface = cursor.u32()?;
            let bytes = cursor.u32()?;
            if bytes % 4 != 0 || bytes > 768 * 4 {
                return Err("keyboard enter array budget".into());
            }
            let mut keys = Vec::with_capacity(bytes as usize / 4);
            for _ in 0..bytes / 4 {
                keys.push(cursor.u32()?);
            }
            KeyboardEvent::Enter(surface, keys)
        }
        2 => {
            cursor.u32()?;
            KeyboardEvent::Leave(cursor.u32()?)
        }
        3 => {
            let serial = cursor.u32()?;
            cursor.u32()?; // Server timestamps have an unrelated, wrapping epoch.
            let key = cursor.u32()?;
            let state = cursor.u32()?;
            if state > 1 {
                return Err("invalid keyboard state for v5-v7".into());
            }
            KeyboardEvent::Key(serial, key, state == 1)
        }
        4 => {
            cursor.u32()?;
            KeyboardEvent::Modifiers(Modifiers {
                depressed: cursor.u32()?,
                latched: cursor.u32()?,
                locked: cursor.u32()?,
                group: cursor.u32()?,
            })
        }
        5 => KeyboardEvent::Timing(cursor.i32()?, cursor.i32()?),
        _ => return Err("unknown keyboard event".into()),
    };
    cursor.finish()?;
    Ok(event)
}

fn read_keymap(fd: OwnedFd, format: u32, size: u32) -> Result<Keymap> {
    if format != 1 {
        return Err(format!("unsupported keymap format {format}"));
    }
    if size == 0 || size > 1024 * 1024 {
        return Err("keymap byte budget".into());
    }
    let file = File::from(fd);
    let metadata = file.metadata().map_err(error)?;
    if !metadata.is_file() || metadata.len() < u64::from(size) {
        return Err("keymap must be a regular file covering its advertised size".into());
    }
    let mut bytes = vec![0; size as usize];
    // Positioned reads do not move the compositor's shared file offset, and
    // truncation is an I/O error rather than a mapped-file SIGBUS.
    file.read_exact_at(&mut bytes, 0).map_err(error)?;
    if bytes.last() != Some(&0) {
        return Err("keymap requires a trailing NUL".into());
    }
    let source = std::str::from_utf8(&bytes).map_err(error)?;
    Keymap::parse(source).map_err(error)
}

fn paint_notice(raster: &mut Raster<'_, '_>, geometry: Geometry, text: &str) {
    let (width, height) = geometry.dimensions();
    let y = if height >= 160 { 48 } else { 0 };
    let clip = Rect {
        x: 0,
        y,
        width: width as u32,
        height: (height as u32).saturating_sub(y as u32).min(96),
    };
    raster.draw(Draw {
        clip,
        primitive: Primitive::Fill {
            rect: clip,
            color: CHROME,
        },
    });
    let columns = width.saturating_sub(16).checked_div(8).unwrap_or(0).max(1);
    let mut column = 0;
    let mut row = 0;
    for scalar in text.chars().take(512) {
        if scalar == '\n' || column == columns {
            row += 1;
            column = 0;
        }
        if row >= 6 {
            break;
        }
        if scalar == '\n' {
            continue;
        }
        raster.draw(Draw {
            clip,
            primitive: Primitive::Glyph {
                x: 8 + column as i64 * 8,
                y: y + row * 16,
                scalar,
                style: GlyphStyle::medium(INK, CHROME),
            },
        });
        column += 1;
    }
}

fn backing_file(directory: &Path, size: usize) -> Result<File> {
    for _ in 0..64 {
        let serial = NEXT_FILE
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| "pool name counter exhausted")?;
        let path = directory.join(format!(".td-editor-shm-{}-{serial}", std::process::id()));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                std::fs::remove_file(&path)
                    .map_err(|e| format!("unlink pool {}: {e}", path.display()))?;
                file.set_len(size as u64).map_err(error)?;
                return Ok(file);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(format!(
                    "create Wayland pool in {}: {e}",
                    directory.display()
                ))
            }
        }
    }
    Err("Wayland pool filename collision budget".into())
}

/// Run the scratch fixture using the normal Wayland environment.
pub fn preview() -> io::Result<()> {
    preview_with_profile(Profile::Windows)
}

pub fn preview_with_profile(profile: Profile) -> io::Result<()> {
    let work = || -> Result<()> {
        let endpoint = endpoint(
            std::env::var_os("WAYLAND_SOCKET"),
            std::env::var_os("WAYLAND_DISPLAY"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )?;
        let stream = connect(endpoint)?;
        let mut window = Window::new(stream, std::env::temp_dir())?;
        window.ui.dispatch(Event::Profile(profile)).map_err(error)?;
        window.run()
    };
    work().map_err(io::Error::other)
}

fn prepare_files(
    profile: Profile,
    paths: Vec<PathBuf>,
    dictionary: Option<PathBuf>,
) -> Result<(
    Controller,
    crate::session::Session,
    crate::spelling::WindowState,
)> {
    if paths.len() > 64 {
        return Err("at most 64 input paths".into());
    }
    let mut files = crate::session::Session::start()?;
    let mut ui = Controller::default();
    for path in paths {
        files.initial_open(&mut ui, path)?;
    }
    if let Some(path) = dictionary {
        files.initial_dictionary(&mut ui, path)?;
    }
    if ui.editor().active().is_none() {
        ui.dispatch(Event::New).map_err(error)?;
    }
    ui.dispatch(Event::Profile(profile)).map_err(error)?;
    ui.dispatch(Event::Focus(false)).map_err(error)?;
    let mut spelling = crate::spelling::WindowState::default();
    if let Some(dictionary) = files.take_dictionary() {
        spelling.install(dictionary);
    }
    Ok((ui, files, spelling))
}

/// Literal startup inputs; control grants read access to every open document.
pub struct FileWindowOptions {
    pub profile: Profile,
    pub paths: Vec<PathBuf>,
    pub dictionary: Option<PathBuf>,
    pub control: Option<PathBuf>,
}

/// Experimental file window; ordinary $EDITOR invocation remains unavailable.
pub fn file_window(options: FileWindowOptions) -> io::Result<()> {
    let FileWindowOptions {
        profile,
        paths,
        dictionary,
        control,
    } = options;
    let work = || -> Result<()> {
        let socket = control
            .map(|path| crate::control_socket::Socket::bind(&path))
            .transpose()
            .map_err(error)?;
        let (ui, files, spelling) = prepare_files(profile, paths, dictionary)?;
        let endpoint = endpoint(
            std::env::var_os("WAYLAND_SOCKET"),
            std::env::var_os("WAYLAND_DISPLAY"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )?;
        let mut window = Window::new(connect(endpoint)?, std::env::temp_dir())?;
        window.ui = ui;
        window.labels.clear();
        let dictionary_notice = spelling.dictionary_entries().map_or_else(
            || "No dictionary selected. ".into(),
            |count| format!("Dictionary loaded: {count} entries. "),
        );
        window.spelling = spelling;
        window.files = Some(files);
        window.control = socket
            .map(crate::control_worker::Worker::start)
            .transpose()
            .map_err(error)?;
        window.notify(format!("{dictionary_notice}Experimental file window. UTF-8 clipboard needs data-device v3. F7 checks spelling; Format > Dictionary selects a local word list. No recovery. Not ready for $EDITOR."));
        let result = window.run();
        match window.finish_control(result) {
            Err(detail) if window.files.as_ref().is_some_and(|files| files.busy()) => Err(format!(
                "{detail}; file operation was pending and may have published. Verify the destination; unsaved edits are not recovered."
            )),
            result => result,
        }
    };
    work().map_err(io::Error::other)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;

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

    fn fixture() -> (Window, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        b.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
        let mut window = Window::new(a, std::env::temp_dir()).unwrap();
        for event in [
            global(30, "wl_compositor", 6),
            global(20, "wl_shm", 1),
            global(90, "xdg_wm_base", 5),
            global(70, "ignored_optional", 42),
        ] {
            window.event(event).unwrap();
        }
        window.event(message(SYNC, 0, &[123])).unwrap();
        window.event(message(DISPLAY, 1, &[SYNC])).unwrap();
        window.event(message(SHM, 0, &[1])).unwrap();
        (window, b)
    }

    fn configure(w: &mut Window, width: u32, height: u32) {
        w.event(message(TOPLEVEL, 0, &[width, height, 0])).unwrap();
        w.event(message(XDG_SURFACE, 0, &[77])).unwrap();
    }

    fn drain(peer: &UnixStream) -> (Vec<Message>, Vec<File>) {
        let mut bytes = Vec::new();
        let mut files = Vec::new();
        loop {
            let mut buf = [0; 16384];
            match crate::sys::receive_for_test(peer, &mut buf) {
                Ok((0, _)) => break,
                Ok((n, fds)) => {
                    bytes.extend_from_slice(&buf[..n]);
                    files.extend(fds.into_iter().map(File::from));
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break
                }
                Err(e) => panic!("{e}"),
            }
        }
        let mut messages = Vec::new();
        while let Some(m) = wire::take(&mut bytes).unwrap() {
            messages.push(m);
        }
        assert!(bytes.is_empty());
        (messages, files)
    }

    fn done(w: &mut Window) {
        let id = w.callback.unwrap();
        w.event(message(id, 0, &[0])).unwrap();
        w.event(message(DISPLAY, 1, &[id])).unwrap();
    }

    fn seat_fixture() -> (Window, UnixStream, u32) {
        let (a, b) = UnixStream::pair().unwrap();
        b.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
        let mut w = Window::new(a, std::env::temp_dir()).unwrap();
        for event in [
            global(1, "wl_compositor", 4),
            global(2, "wl_shm", 1),
            global(3, "xdg_wm_base", 1),
            global(4, "wl_seat", 10),
        ] {
            w.event(event).unwrap();
        }
        w.event(message(SYNC, 0, &[0])).unwrap();
        let seat = w.seat.unwrap();
        let (binds, _) = drain(&b);
        let binding = binds
            .iter()
            .find(|m| {
                m.object == REGISTRY && {
                    let mut c = Cursor::new(&m.payload);
                    c.u32().unwrap() == 4
                }
            })
            .unwrap();
        let mut c = Cursor::new(&binding.payload);
        assert_eq!(c.u32().unwrap(), 4);
        assert_eq!(c.string().unwrap(), "wl_seat");
        assert_eq!(c.u32().unwrap(), 7);
        assert_eq!(c.u32().unwrap(), seat);
        assert!(w.device.is_none());
        w.event(message(seat, 0, &[3])).unwrap();
        let device = w.device.unwrap();
        assert_eq!(
            drain(&b).0,
            [
                message(seat, 0, &[w.pointer.device.unwrap()]),
                message(seat, 1, &[device])
            ]
        );
        (w, b, device)
    }

    fn map_file() -> File {
        let source = include_str!("../tests/fixtures/us.xkb");
        let file = backing_file(&std::env::temp_dir(), source.len() + 1).unwrap();
        file.write_all_at(source.as_bytes(), 0).unwrap();
        file
    }

    fn pointer_move(w: &mut Window, x: i64, y: i64) {
        let pointer = w.pointer.device.unwrap();
        w.event(message(
            pointer,
            2,
            &[0, (x * 256) as u32, (y * 256) as u32],
        ))
        .unwrap();
    }
    fn pointer_button(w: &mut Window, pressed: bool) {
        let pointer = w.pointer.device.unwrap();
        w.event(message(pointer, 3, &[1, 0, 0x110, u32::from(pressed)]))
            .unwrap();
    }
    fn pointer_enter(w: &mut Window) {
        let pointer = w.pointer.device.unwrap();
        w.event(message(pointer, 0, &[19, SURFACE, 0, 0])).unwrap();
    }

    #[test]
    fn pointer_selects_scalars_drags_and_leaves_without_keyboard_focus() {
        let (mut w, _peer, _) = seat_fixture();
        w.ui = Controller::default();
        w.ui.dispatch(Event::Load("abé中z\nnext".as_bytes()))
            .unwrap();
        w.ui.dispatch(Event::Focus(false)).unwrap();
        let tab = w.ui.editor().active().unwrap();
        let area = w.ui.geometry().document();
        pointer_enter(&mut w);
        let pointer = w.pointer.device.unwrap();
        // Exact midpoint chooses the preceding boundary; one 24.8 unit past
        // it chooses the following boundary without waiting for keyboard focus.
        w.event(message(
            pointer,
            2,
            &[0, ((area.x + 4) * 256) as u32, (area.y * 256) as u32],
        ))
        .unwrap();
        pointer_button(&mut w, true);
        assert_eq!(w.ui.editor().document(tab).unwrap().selection().caret, 0);
        pointer_button(&mut w, false);
        w.event(message(
            pointer,
            2,
            &[0, ((area.x + 4) * 256 + 1) as u32, (area.y * 256) as u32],
        ))
        .unwrap();
        pointer_button(&mut w, true);
        assert_eq!(w.ui.editor().document(tab).unwrap().selection().anchor, 1);
        pointer_move(&mut w, area.x + 24, area.y);
        assert_eq!(
            w.ui.editor().document(tab).unwrap().selection(),
            crate::model::Selection {
                anchor: 1,
                caret: 4
            }
        );
        // Implicit grabs can report signed out-of-surface motion without leave.
        w.event(message(pointer, 2, &[0, i32::MIN as u32, i32::MIN as u32]))
            .unwrap();
        assert_eq!(
            w.ui.editor().document(tab).unwrap().selection(),
            crate::model::Selection {
                anchor: 1,
                caret: 0
            }
        );
        w.event(message(pointer, 2, &[0, i32::MAX as u32, i32::MAX as u32]))
            .unwrap();
        assert_eq!(
            w.ui.editor().document(tab).unwrap().selection().caret,
            w.ui.editor().document(tab).unwrap().text().len()
        );
        w.event(message(pointer, 1, &[20, SURFACE])).unwrap();
        let selection = w.ui.editor().document(tab).unwrap().selection();
        pointer_move(&mut w, area.x, area.y + 16);
        pointer_button(&mut w, false);
        assert_eq!(w.ui.editor().document(tab).unwrap().selection(), selection);
        assert!(!w.ui.focused());
        assert_eq!(w.ui.editor().document(tab).unwrap().text(), "abé中z\nnext");
    }

    #[test]
    fn pointer_cursor_is_one_immutable_argb_pool_and_reuses_enter_serials() {
        let (mut w, peer, _) = seat_fixture();
        pointer_enter(&mut w);
        assert!(w.cursor_image.is_none());
        w.event(message(SHM, 0, &[0])).unwrap();
        let image = w.cursor_image.as_ref().unwrap();
        let (surface, buffer) = (image.surface, image.buffer);
        let pointer = w.pointer.device.unwrap();
        let (requests, files) = drain(&peer);
        assert_eq!(files.len(), 1);
        assert_eq!(
            requests.last().unwrap(),
            &message(pointer, 0, &[19, surface, 0, 0])
        );
        let file = &files[0];
        use std::os::unix::fs::MetadataExt;
        assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(file.metadata().unwrap().nlink(), 0);
        let mut bytes = [0; 1536];
        file.read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(bytes, cursor_pixels());
        assert!(bytes.as_chunks::<4>().0.contains(&[0, 0, 0, 0]));
        assert!(bytes.as_chunks::<4>().0.contains(&[0x3f, 0x45, 0x48, 0xff]));
        assert!(w.buffers.is_empty());
        w.event(message(SHM, 0, &[1])).unwrap();
        w.event(message(SHM, 0, &[0x34325258])).unwrap();
        assert!(drain(&peer).0.is_empty());
        w.event(message(buffer, 0, &[])).unwrap();
        assert!(w.event(message(buffer, 0, &[])).is_err());
        w.event(message(pointer, 1, &[20, SURFACE])).unwrap();
        w.event(message(pointer, 0, &[21, SURFACE, 0, 0])).unwrap();
        let (requests, files) = drain(&peer);
        assert!(files.is_empty());
        assert_eq!(requests, [message(pointer, 0, &[21, surface, 0, 0])]);
    }

    #[test]
    fn pointer_wheel_is_framed_and_does_not_retarget_across_tabs() {
        let (mut w, _peer, _) = seat_fixture();
        w.ui = Controller::default();
        let text = "abcdefghijklmnopqrstuvwxyz\n".repeat(100);
        w.ui.dispatch(Event::Load(text.as_bytes())).unwrap();
        let first = w.ui.editor().active().unwrap();
        w.ui.dispatch(Event::Load(text.as_bytes())).unwrap();
        let second = w.ui.editor().active().unwrap();
        w.ui.dispatch(Event::Resize {
            width: 280,
            height: 160,
            scale: 1,
        })
        .unwrap();
        pointer_enter(&mut w);
        let pointer = w.pointer.device.unwrap();
        w.event(message(pointer, 8, &[0, 1])).unwrap();
        w.event(message(pointer, 4, &[0, 0, 10000])).unwrap();
        assert_eq!(w.ui.tab_view(second).unwrap().viewport.origin().row, 0);
        w.event(message(pointer, 5, &[])).unwrap();
        assert_eq!(w.ui.tab_view(second).unwrap().viewport.origin().row, 3);
        w.event(message(pointer, 4, &[0, 0, 15 * 256])).unwrap();
        w.event(message(pointer, 5, &[])).unwrap();
        w.ui.dispatch(Event::SelectTab(first)).unwrap();
        w.event(message(pointer, 4, &[0, 0, 256])).unwrap();
        w.event(message(pointer, 5, &[])).unwrap();
        assert_eq!(w.ui.tab_view(first).unwrap().viewport.origin().row, 0);
        w.event(message(pointer, 4, &[0, 0, 16 * 256])).unwrap();
        w.ui.dispatch(Event::SelectTab(second)).unwrap();
        w.event(message(pointer, 5, &[])).unwrap();
        assert_eq!(w.ui.tab_view(second).unwrap().viewport.origin().row, 3);
        assert_eq!(w.ui.tab_view(first).unwrap().viewport.origin().row, 0);
    }

    #[test]
    fn pointer_shift_selection_and_capability_replacement_are_independent_of_keyboard() {
        let (mut w, peer, device) = seat_fixture();
        send_map(&mut w, &peer, device, &map_file());
        focus(&mut w, device);
        w.ui = Controller::default();
        w.ui.dispatch(Event::Load(b"abcdef")).unwrap();
        let tab = w.ui.editor().active().unwrap();
        let area = w.ui.geometry().document();
        pointer_enter(&mut w);
        pointer_move(&mut w, area.x + 8, area.y);
        pointer_button(&mut w, true);
        pointer_button(&mut w, false);
        w.event(message(device, 4, &[22, 1, 0, 0, 0])).unwrap();
        pointer_move(&mut w, area.x + 32, area.y);
        pointer_button(&mut w, true);
        assert_eq!(
            w.ui.editor().document(tab).unwrap().selection(),
            crate::model::Selection {
                anchor: 1,
                caret: 4
            }
        );
        let old = w.pointer.device.unwrap();
        let seat = w.seat.unwrap();
        w.event(message(seat, 0, &[2])).unwrap();
        assert!(w.pointer.device.is_none());
        assert!(w.ui.focused());
        assert!(w.input.focused && w.input.synchronized);
        let selection = w.ui.editor().document(tab).unwrap().selection();
        w.event(message(old, 2, &[0, 0, 0])).unwrap();
        assert_eq!(w.ui.editor().document(tab).unwrap().selection(), selection);
        assert!(w.event(message(old, 3, &[0, 0, 0x110, 2])).is_err());
        w.event(message(seat, 0, &[3])).unwrap();
        assert_ne!(w.pointer.device, Some(old));
        w.event(message(DISPLAY, 1, &[old])).unwrap();
        assert_eq!(w.kind(old).unwrap(), Kind::Free);
        assert!(drain(&peer).0.iter().any(|m| *m == message(old, 1, &[])));
        pointer_enter(&mut w);
        w.input.synchronized = false;
        pointer_move(&mut w, area.x + 16, area.y);
        pointer_button(&mut w, true);
        assert_eq!(
            w.ui.editor().document(tab).unwrap().selection(),
            crate::model::Selection {
                anchor: 2,
                caret: 2
            }
        );
    }

    #[test]
    fn pointer_tab_close_is_revision_bound_and_modal_clicks_never_confirm() {
        let (mut w, _peer) = file_dialog_fixture();
        w.ui.dispatch(Event::New).unwrap();
        let active = w.ui.editor().active().unwrap();
        w.ui.dispatch(Event::Edit {
            tab: active,
            revision: 0,
            command: crate::model::Command::Insert("keep".into()),
        })
        .unwrap();
        pointer_enter(&mut w);
        let close = w.ui.geometry().tab_close(1, 1, 2).unwrap();
        pointer_move(&mut w, close.x, close.y);
        pointer_button(&mut w, true);
        assert!(w.closing.is_some());
        let before = format!("{:?}", w.ui.editor());
        for _ in 0..2 {
            pointer_button(&mut w, false);
            pointer_button(&mut w, true);
        }
        let area = w.ui.geometry().document();
        pointer_move(&mut w, area.x, area.y);
        pointer_button(&mut w, false);
        pointer_button(&mut w, true);
        assert_eq!(format!("{:?}", w.ui.editor()), before);
        assert!(!w.closed);
        w.chord("Escape", false).unwrap();
        pointer_move(&mut w, 799, 599);
        assert_eq!(format!("{:?}", w.ui.editor()), before);
    }

    #[test]
    fn opening_a_dialog_cancels_controller_drag_before_another_pointer_event() {
        let (mut w, _peer) = file_dialog_fixture();
        pointer_enter(&mut w);
        let area = w.ui.geometry().document();
        pointer_move(&mut w, area.x, area.y);
        pointer_button(&mut w, true);
        w.close();
        let tab = w.ui.editor().active().unwrap();
        let revision = w.ui.editor().document(tab).unwrap().revision();
        assert_eq!(
            w.ui.dispatch(Event::Pointer {
                tab,
                revision,
                phase: crate::ui::PointerPhase::Move,
                x: 799,
                y: 599,
                extend: false,
            })
            .unwrap(),
            Outcome::Ignored
        );
    }

    #[test]
    fn pointer_clicks_switch_tabs_and_close_an_inactive_dirty_tab_without_selecting_it() {
        for profile in [Profile::Windows, Profile::Emacs] {
            let (mut w, _peer) = file_dialog_fixture();
            w.ui.dispatch(Event::Profile(profile)).unwrap();
            let first = w.ui.editor().active().unwrap();
            w.chord("x", false).unwrap();
            w.ui.dispatch(Event::New).unwrap();
            let second = w.ui.editor().active().unwrap();
            pointer_enter(&mut w);
            let rect = w.ui.geometry().tab(0, 1, 2).unwrap();
            pointer_move(&mut w, rect.x, rect.y);
            pointer_button(&mut w, true);
            pointer_button(&mut w, false);
            assert_eq!(w.ui.editor().active(), Some(first));
            let rect = w.ui.geometry().tab(1, 0, 2).unwrap();
            pointer_move(&mut w, rect.x, rect.y);
            pointer_button(&mut w, true);
            pointer_button(&mut w, false);
            assert_eq!(w.ui.editor().active(), Some(second));
            let rect = w.ui.geometry().tab_close(0, 1, 2).unwrap();
            pointer_move(&mut w, rect.x, rect.y);
            pointer_button(&mut w, true);
            assert_eq!(w.ui.editor().active(), Some(second));
            assert_eq!(
                w.closing.as_ref().unwrap().next(w.ui.editor()).unwrap(),
                Some(Target {
                    tab: first,
                    revision: 1
                })
            );
            w.chord("C-d", false).unwrap();
            assert_eq!(w.ui.editor().active(), Some(second));
            assert!(w.ui.editor().document(first).is_err());
            assert!(!w.closed);
        }
    }

    fn send_map(w: &mut Window, peer: &UnixStream, device: u32, file: &File) {
        let mut sender = Connection::new(peer.try_clone().unwrap()).unwrap();
        let mut body = Builder::new();
        body.u32(1);
        body.u32(file.metadata().unwrap().len() as u32);
        sender.send(device, 0, body, Some(file)).unwrap();
        w.connection.read_more().unwrap();
        while let Some(event) = wire::take(&mut w.connection.pending).unwrap() {
            w.event(event).unwrap();
        }
    }

    fn focus(w: &mut Window, device: u32) {
        w.event(message(device, 1, &[1, SURFACE, 0])).unwrap();
        w.event(message(device, 4, &[2, 0, 0, 0, 0])).unwrap();
        w.event(message(device, 5, &[25, 600])).unwrap();
    }

    fn key(w: &mut Window, device: u32, code: u32) {
        w.event(message(device, 3, &[1, u32::MAX, code, 1]))
            .unwrap();
        w.event(message(device, 3, &[2, 0, code, 0])).unwrap();
    }

    fn text_event(object: u32, opcode: u16, text: &str) -> Message {
        let mut body = Builder::new();
        body.string(text).unwrap();
        wire::take(&mut body.message(object, opcode).unwrap())
            .unwrap()
            .unwrap()
    }

    fn find_fixture(profile: Profile) -> (Window, UnixStream, u32) {
        let (mut w, peer, keyboard) = seat_fixture();
        w.ui = Controller::default();
        w.labels.clear();
        w.ui.dispatch(Event::Load(b"one two one")).unwrap();
        w.ui.dispatch(Event::Profile(profile)).unwrap();
        send_map(&mut w, &peer, keyboard, &map_file());
        focus(&mut w, keyboard);
        w.notice = None;
        (w, peer, keyboard)
    }

    #[test]
    fn native_find_keys_submit_in_both_profiles_and_windows_f3_reports_end_before_wrap() {
        for profile in [Profile::Windows, Profile::Emacs] {
            let (mut w, _peer, keyboard) = find_fixture(profile);
            w.event(message(keyboard, 4, &[0, 4, 0, 0, 0])).unwrap();
            key(
                &mut w,
                keyboard,
                if profile == Profile::Windows { 33 } else { 31 },
            );
            assert!(w.search.is_some());
            w.event(message(keyboard, 4, &[0, 0, 0, 0, 0])).unwrap();
            for code in [24, 49, 18] {
                key(&mut w, keyboard, code);
            }
            assert_eq!(w.search.as_ref().unwrap().text, "one");
            assert_eq!(w.ui.editor().document(1).unwrap().selection().range(), 0..0);
            key(&mut w, keyboard, 28);
            assert!(w.search.is_none());
            assert_eq!(w.ui.editor().document(1).unwrap().selection().range(), 0..3);
            if profile == Profile::Windows {
                key(&mut w, keyboard, 61);
                assert_eq!(
                    w.ui.editor().document(1).unwrap().selection().range(),
                    8..11
                );
                key(&mut w, keyboard, 61);
                assert!(w.notice.as_ref().unwrap().contains("Reached end"));
                assert_eq!(
                    w.ui.editor().document(1).unwrap().selection().range(),
                    8..11
                );
                key(&mut w, keyboard, 61);
                assert!(w.notice.as_ref().unwrap().contains("wrapped"));
                assert_eq!(w.ui.editor().document(1).unwrap().selection().range(), 0..3);
                w.event(message(keyboard, 4, &[0, 1, 0, 0, 0])).unwrap();
                key(&mut w, keyboard, 61); // Shift+F3
                assert!(w.notice.as_ref().unwrap().contains("Reached start"));
                key(&mut w, keyboard, 61);
                assert_eq!(
                    w.ui.editor().document(1).unwrap().selection().range(),
                    8..11
                );
            } else {
                w.event(message(keyboard, 4, &[0, 4, 0, 0, 0])).unwrap();
                key(&mut w, keyboard, 31);
                assert_eq!(w.search.as_ref().unwrap().text, "one");
                key(&mut w, keyboard, 31); // explicit C-s submits, no live incremental edits
                assert_eq!(
                    w.ui.editor().document(1).unwrap().selection().range(),
                    8..11
                );
                key(&mut w, keyboard, 19);
                assert!(w.search.as_ref().unwrap().backward);
                key(&mut w, keyboard, 19);
                assert_eq!(w.ui.editor().document(1).unwrap().selection().range(), 0..3);
            }
            assert!(!w.ui.editor().document(1).unwrap().dirty());
            assert_eq!(w.ui.editor().document(1).unwrap().history_depth(), (0, 0));
            assert!(w.input.repeat(1000).unwrap().is_none());
        }
    }

    #[test]
    fn find_prompt_cancel_repeat_staleness_and_input_loss_do_not_edit_or_retarget() {
        let (mut w, _peer, keyboard) = find_fixture(Profile::Emacs);
        w.search_request("find", 1, 0).unwrap();
        w.chord("x", true).unwrap();
        assert!(w.search.as_ref().unwrap().text.is_empty());
        w.chord("Return", false).unwrap();
        assert!(w.search.is_some());
        w.chord("C-r", false).unwrap();
        assert!(w.search.as_ref().unwrap().backward);
        w.chord("λ", false).unwrap();
        w.chord("C-g", false).unwrap();
        assert!(w.search.is_none());
        assert_eq!(w.ui.editor().document(1).unwrap().selection().range(), 0..0);
        w.search_request("find", 1, 0).unwrap();
        w.chord("o", false).unwrap();
        w.event(message(keyboard, 2, &[0, SURFACE])).unwrap();
        assert!(w.search_notice().unwrap().contains("paused"));
        assert_eq!(w.search.as_ref().unwrap().text, "o");
        focus(&mut w, keyboard);
        w.ui.dispatch(Event::New).unwrap();
        w.chord("Return", false).unwrap();
        assert!(w.notice.as_ref().unwrap().contains("Find cancelled"));
        assert_eq!(w.ui.editor().active(), Some(2));
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "one two one");
        assert_eq!(w.ui.editor().document(2).unwrap().text(), "");
    }

    #[test]
    fn find_menu_prompt_changes_only_overlay_pixels_and_restores_them_on_cancel() {
        let (mut w, peer, _) = find_fixture(Profile::Windows);
        configure(&mut w, 800, 600);
        w.event(message(SHM, 0, &[1])).unwrap();
        w.draw().unwrap();
        let original = w.pixels.clone();
        done(&mut w);
        drain(&peer);
        w.open_menu(crate::menu::Group::Edit).unwrap();
        let index = crate::menu::Group::Edit
            .items()
            .iter()
            .position(|item| *item == crate::menu::Item::Find)
            .unwrap();
        w.activate_menu(index).unwrap();
        assert!(w.search.is_some());
        w.draw().unwrap();
        assert!(w.pixels != original);
        done(&mut w);
        drain(&peer);
        w.chord("Escape", false).unwrap();
        w.draw().unwrap();
        assert!(w.pixels == original);
        assert_eq!(w.ui.editor().document(1).unwrap().history_depth(), (0, 0));
    }

    #[test]
    #[allow(clippy::unreachable)]
    fn native_find_wrap_is_invalidated_by_event_tick_focus_keymap_cancel_and_close() {
        for transition in [
            "event", "tick", "focus", "keymap", "seat", "cancel", "close",
        ] {
            let (mut w, peer, keyboard) = find_fixture(Profile::Windows);
            w.find(1, 0, "zzz", false);
            assert!(w.notice.as_ref().unwrap().contains("Reached end"));
            match transition {
                "event" => {
                    key(&mut w, keyboard, 106); // Right then Left through native event wrapper
                    key(&mut w, keyboard, 105);
                }
                "tick" => {
                    for (at, now) in [(1, 1), (0, 2)] {
                        w.ui.dispatch(Event::Edit {
                            tab: 1,
                            revision: 0,
                            command: crate::model::Command::Select(crate::model::Selection {
                                anchor: at,
                                caret: at,
                            }),
                        })
                        .unwrap();
                        w.tick(now, false).unwrap();
                    }
                }
                "focus" => {
                    w.event(message(keyboard, 2, &[0, SURFACE])).unwrap();
                    focus(&mut w, keyboard);
                }
                "keymap" => send_map(&mut w, &peer, keyboard, &map_file()),
                "seat" => w.release_keyboard().unwrap(),
                "cancel" => {
                    w.chord("Escape", false).unwrap();
                }
                "close" => w.close(),
                _ => unreachable!(),
            }
            assert_eq!(w.ui.editor().document(1).unwrap().selection().range(), 0..0);
            w.find(1, 0, "zzz", false);
            assert!(
                w.notice.as_ref().unwrap().contains("Reached end"),
                "{transition}"
            );
        }
    }

    #[test]
    fn native_find_prompt_blocks_pointer_and_repeats_and_cancels_on_close() {
        let (mut w, _peer, keyboard) = find_fixture(Profile::Windows);
        w.chord("C-f", true).unwrap();
        assert!(w.search.is_none());
        w.find(1, 0, "zzz", false);
        let notice = w.notice.clone();
        w.chord("F3", true).unwrap();
        assert_eq!(w.notice, notice);
        w.search_request("find", 1, 0).unwrap();
        pointer_enter(&mut w);
        let area = w.ui.geometry().document();
        pointer_move(&mut w, area.x + 56, area.y);
        pointer_button(&mut w, true);
        pointer_move(&mut w, area.x + 72, area.y);
        pointer_button(&mut w, false);
        assert_eq!(w.ui.editor().document(1).unwrap().selection().range(), 0..0);
        assert!(w.search.is_some());
        key(&mut w, keyboard, 1); // Escape
        w.search_request("find", 1, 0).unwrap();
        w.close();
        assert!(w.search.is_none());
    }

    #[test]
    fn native_find_open_cancels_prefix_drag_repeat_and_pending_paste_and_rejects_multiline_seed() {
        let (mut w, _peer, keyboard) = find_fixture(Profile::Emacs);
        pointer_enter(&mut w);
        let area = w.ui.geometry().document();
        pointer_move(&mut w, area.x, area.y);
        pointer_button(&mut w, true);
        assert!(w.pointer.held);
        w.ui.dispatch(Event::Key {
            tab: 1,
            revision: 0,
            chord: "C-x",
        })
        .unwrap();
        assert!(w.ui.keys().pending());
        w.input.key(106, true).unwrap();
        w.input.arm(106, 0);
        let (incoming, _producer) =
            crate::transfer::Incoming::begin(w.ui.editor(), 1, 0, 0).unwrap();
        w.clipboard.incoming = Some(incoming);
        w.search_request("find", 1, 0).unwrap();
        assert!(!w.ui.keys().pending());
        assert!(!w.pointer.held);
        assert!(w.input.repeat(1000).unwrap().is_none());
        assert!(w.clipboard.incoming.is_none());
        w.chord("Escape", false).unwrap();
        w.ui.dispatch(Event::Load(b"one\ntwo")).unwrap();
        w.ui.dispatch(Event::Edit {
            tab: 2,
            revision: 0,
            command: crate::model::Command::Select(crate::model::Selection {
                anchor: 0,
                caret: 7,
            }),
        })
        .unwrap();
        w.search_request("find", 2, 0).unwrap();
        assert!(w.search.as_ref().unwrap().text.is_empty());
        w.event(message(keyboard, 2, &[0, SURFACE])).unwrap();
        assert!(w.search.is_some());
    }

    #[test]
    fn number_menu_uses_logical_lines_in_both_profiles_and_refuses_invalid_entry() {
        for profile in [Profile::Windows, Profile::Emacs] {
            let (mut w, _peer, keyboard) = find_fixture(profile);
            w.ui = Controller::default();
            w.ui.dispatch(Event::Load("é long first line\nsecond\n".as_bytes()))
                .unwrap();
            w.ui.dispatch(Event::Profile(profile)).unwrap();
            configure(&mut w, 800, 600);
            w.open_menu(crate::menu::Group::Edit).unwrap();
            let index = crate::menu::Group::Edit
                .items()
                .iter()
                .position(|item| *item == crate::menu::Item::GoToLine)
                .unwrap();
            w.activate_menu(index).unwrap();
            assert!(w.number.is_some());
            assert!(w.pointer_modal());
            w.chord("2", true).unwrap();
            key(&mut w, keyboard, 28);
            assert!(w.number_notice().unwrap().contains("existing line"));
            assert_eq!(w.ui.editor().document(1).unwrap().selection().caret, 0);
            key(&mut w, keyboard, 4); // 3: final empty logical line
            key(&mut w, keyboard, 28);
            assert!(w.number.is_none());
            assert_eq!(w.notice.as_deref(), Some("Moved to logical line."));
            let doc = w.ui.editor().document(1).unwrap();
            assert_eq!(doc.selection().caret, doc.text().len());
            assert!(!doc.dirty());
            assert_eq!(doc.history_depth(), (0, 0));
            w.number_request(1, 0, crate::number::Kind::Line).unwrap();
            key(&mut w, keyboard, 5); // 4: nonexistent
            key(&mut w, keyboard, 28);
            assert!(w.number.is_some());
            assert!(w.number_notice().unwrap().contains("existing line"));
            let selection = w.ui.editor().document(1).unwrap().selection();
            w.chord("C-g", false).unwrap();
            assert!(w.number.is_none());
            assert_eq!(w.ui.editor().document(1).unwrap().selection(), selection);
        }
    }

    #[test]
    fn number_focus_pause_stale_target_and_overlay_are_nonediting() {
        let (mut w, peer, keyboard) = find_fixture(Profile::Windows);
        configure(&mut w, 800, 600);
        w.event(message(SHM, 0, &[1])).unwrap();
        w.draw().unwrap();
        let baseline = w.pixels.clone();
        done(&mut w);
        drain(&peer);
        w.search_request("find", 1, 0).unwrap();
        w.number_request(1, 0, crate::number::Kind::Line).unwrap();
        assert!(w.search.is_none());
        w.draw().unwrap();
        assert!(w.pixels != baseline);
        done(&mut w);
        drain(&peer);
        w.chord("Escape", false).unwrap();
        w.draw().unwrap();
        assert!(w.pixels == baseline);
        done(&mut w);
        drain(&peer);
        w.number_request(1, 0, crate::number::Kind::Line).unwrap();
        w.chord("1", false).unwrap();
        w.event(message(keyboard, 2, &[0, SURFACE])).unwrap();
        assert!(w.number_notice().unwrap().contains("paused"));
        focus(&mut w, keyboard);
        w.ui.dispatch(Event::New).unwrap();
        w.chord("Return", false).unwrap();
        assert!(w.number.is_none());
        assert!(w.notice.as_ref().unwrap().contains("Go To Line cancelled"));
        assert_eq!(w.ui.editor().active(), Some(2));
        w.number_request(2, 0, crate::number::Kind::Line).unwrap();
        w.close();
        assert!(w.number.is_none());
        assert!(w.closed);
    }

    #[test]
    fn number_f6_works_below_menu_height_and_paused_invalid_feedback_is_visible() {
        for profile in [Profile::Windows, Profile::Emacs] {
            let (mut w, peer, keyboard) = find_fixture(profile);
            configure(&mut w, 320, 240);
            w.open_menu(crate::menu::Group::Edit).unwrap();
            assert!(w.menu.is_none());
            w.chord("F6", true).unwrap();
            assert!(w.number.is_none());
            key(&mut w, keyboard, 64); // F6 despite unavailable Edit panel
            assert!(w.number.is_some());
            key(&mut w, keyboard, 11); // zero
            key(&mut w, keyboard, 28);
            assert!(w.number_notice().unwrap().contains("Invalid line number"));
            w.event(message(keyboard, 2, &[0, SURFACE])).unwrap();
            w.event(message(SHM, 0, &[1])).unwrap();
            w.draw().unwrap();
            let error_pixels = w.pixels.clone();
            done(&mut w);
            drain(&peer);
            let prompt = w.number.as_mut().unwrap();
            prompt.type_chord("Backspace");
            prompt.type_chord("0"); // same digits/focus, only invalid feedback removed
            w.frames.invalidate(true);
            w.draw().unwrap();
            assert!(w.pixels != error_pixels);
            assert_eq!(w.ui.editor().document(1).unwrap().selection().range(), 0..0);
        }
    }

    fn clipboard_fixture() -> (Window, UnixStream, u32, u32) {
        let (mut w, peer, keyboard) = seat_fixture();
        w.event(global(8, "wl_data_device_manager", 9)).unwrap();
        w.initialize_clipboard().unwrap();
        let device = w.clipboard.device.unwrap();
        let (requests, _) = drain(&peer);
        let mut c = Cursor::new(&requests[0].payload);
        assert_eq!(c.u32().unwrap(), 8);
        assert_eq!(c.string().unwrap(), "wl_data_device_manager");
        assert_eq!(c.u32().unwrap(), 3);
        assert!(!w.required.contains(&8));
        w.ui = Controller::default();
        w.ui.dispatch(Event::Load("é abc\n".as_bytes())).unwrap();
        w.ui.dispatch(Event::Profile(Profile::Windows)).unwrap();
        w.ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: crate::model::Command::Select(crate::model::Selection {
                anchor: 2,
                caret: 0,
            }),
        })
        .unwrap();
        send_map(&mut w, &peer, keyboard, &map_file());
        focus(&mut w, keyboard);
        w.notice = None;
        (w, peer, keyboard, device)
    }

    fn selection_offer(w: &mut Window, device: u32, id: u32, mimes: &[&str]) {
        w.event(message(device, 0, &[id])).unwrap();
        for mime in mimes {
            w.event(text_event(id, 0, mime)).unwrap();
        }
        w.event(message(device, 5, &[id])).unwrap();
    }

    fn source_send(w: &mut Window, peer: &UnixStream, source: u32, mime: &str, file: &File) {
        let mut sender = Connection::new(peer.try_clone().unwrap()).unwrap();
        let mut body = Builder::new();
        body.string(mime).unwrap();
        sender.send(source, 1, body, Some(file)).unwrap();
        w.connection.read_more().unwrap();
        let event = wire::take(&mut w.connection.pending).unwrap().unwrap();
        assert!(w.message_needs_descriptor(&event).unwrap());
        w.event(event).unwrap();
    }

    #[test]
    fn clipboard_copy_and_cut_use_actual_keyboard_serial_in_both_profiles() {
        for profile in [Profile::Windows, Profile::Emacs] {
            let (mut w, peer, keyboard, device) = clipboard_fixture();
            w.ui.dispatch(Event::Profile(profile)).unwrap();
            w.event(message(
                keyboard,
                4,
                &[0, if profile == Profile::Windows { 4 } else { 8 }, 0, 0, 0],
            ))
            .unwrap();
            let code = if profile == Profile::Windows { 46 } else { 17 };
            w.event(message(keyboard, 3, &[1234, 0, code, 1])).unwrap();
            w.event(message(keyboard, 3, &[999, 0, code, 0])).unwrap();
            let source = w.clipboard.source.as_ref().unwrap().0;
            assert_eq!(&*w.clipboard.source.as_ref().unwrap().1, "é");
            let (requests, _) = drain(&peer);
            assert!(requests.contains(&message(device, 1, &[source, 1234])));
            assert!(w.activation_serial.is_none());
            assert_eq!(w.ui.editor().document(1).unwrap().text(), "é abc\n");
            w.event(message(keyboard, 4, &[0, 4, 0, 0, 0])).unwrap();
            key(
                &mut w,
                keyboard,
                if profile == Profile::Windows { 45 } else { 17 },
            );
            assert_eq!(w.ui.editor().document(1).unwrap().text(), " abc\n");
            assert_eq!(w.ui.editor().document(1).unwrap().history_depth(), (1, 0));
            assert_eq!(w.kind(source).unwrap(), Kind::RetiredClipboardSource);
            assert!(w.input.repeat(1000).unwrap().is_none());
            w.ui.dispatch(Event::Edit {
                tab: 1,
                revision: 1,
                command: crate::model::Command::Undo,
            })
            .unwrap();
            assert_eq!(w.ui.editor().document(1).unwrap().text(), "é abc\n");
        }
    }

    #[test]
    fn clipboard_copy_requires_serial_and_optional_global_and_pointer_menu_uses_press() {
        let (mut w, peer, _keyboard, device) = clipboard_fixture();
        w.clipboard_request("cut", 1, 0).unwrap();
        assert!(w.clipboard.source.is_none());
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "é abc\n");
        pointer_enter(&mut w);
        w.open_menu(crate::menu::Group::Edit).unwrap();
        let menu = w.menu.as_ref().unwrap();
        assert!(menu.enabled(crate::menu::Item::Copy));
        let panel = menu.panel(w.ui.geometry()).unwrap();
        pointer_move(&mut w, panel.x + 5, panel.y + 3 * 24 + 5);
        let pointer = w.pointer.device.unwrap();
        w.event(message(pointer, 3, &[777, 0, 0x110, 1])).unwrap();
        let source = w.clipboard.source.as_ref().unwrap().0;
        assert!(drain(&peer).0.contains(&message(device, 1, &[source, 777])));
        w.event(message(REGISTRY, 1, &[8])).unwrap();
        assert!(w.clipboard.manager.is_none());
        assert!(w.clipboard.device.is_none());
        assert!(!w.closed);
        w.open_menu(crate::menu::Group::Edit).unwrap();
        assert!(!w.menu.as_ref().unwrap().enabled(crate::menu::Item::Copy));
    }

    #[test]
    fn clipboard_received_socket_is_nonblocking_bounded_and_serves_immutable_bytes() {
        let (mut w, peer, keyboard, _device) = clipboard_fixture();
        w.event(message(keyboard, 4, &[0, 4, 0, 0, 0])).unwrap();
        key(&mut w, keyboard, 46);
        let source = w.clipboard.source.as_ref().unwrap().0;
        drain(&peer);
        w.ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: crate::model::Command::Insert("changed".into()),
        })
        .unwrap();
        let (mut reader, writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let writer = File::from(OwnedFd::from(writer));
        source_send(&mut w, &peer, source, crate::data::UTF8, &writer);
        drop(writer);
        assert!(w.clipboard.outgoing.is_some());
        w.tick(1, true).unwrap();
        assert!(w.clipboard.outgoing.is_none());
        let mut text = String::new();
        reader.read_to_string(&mut text).unwrap();
        assert_eq!(text, "é");
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "changed abc\n");
    }

    #[test]
    fn clipboard_paste_prefers_utf8_waits_for_eof_and_is_one_undo_transaction() {
        let (mut w, peer, _keyboard, device) = clipboard_fixture();
        selection_offer(
            &mut w,
            device,
            0xff00_0010,
            &[crate::data::PLAIN, crate::data::UTF8],
        );
        w.clipboard_request("paste", 1, 0).unwrap();
        let (requests, mut files) = drain(&peer);
        assert_eq!(requests, [text_event(0xff00_0010, 1, crate::data::UTF8)]);
        let mut writer = files.pop().unwrap();
        assert!(files.is_empty());
        writer.write_all(&[0xe4, 0xb8]).unwrap();
        w.tick(1, true).unwrap();
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "é abc\n");
        writer.write_all(&[0xad, b'\r', b'\n']).unwrap();
        drop(writer);
        w.tick(2, true).unwrap();
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "中\n abc\n");
        assert_eq!(w.ui.editor().document(1).unwrap().history_depth(), (1, 0));
        w.ui.dispatch(Event::Edit {
            tab: 1,
            revision: 1,
            command: crate::model::Command::Undo,
        })
        .unwrap();
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "é abc\n");
    }

    #[test]
    fn clipboard_paste_cancels_on_focus_escape_edit_selection_and_deadline() {
        for action in 0..5 {
            let (mut w, peer, keyboard, device) = clipboard_fixture();
            selection_offer(&mut w, device, 0xff00_0010, &[crate::data::PLAIN]);
            w.clipboard_request("paste", 1, 0).unwrap();
            let (_, mut files) = drain(&peer);
            let mut writer = files.pop().unwrap();
            writer.write_all(b"do not admit").unwrap();
            match action {
                0 => w.event(message(keyboard, 2, &[0, SURFACE])).unwrap(),
                1 => {
                    w.chord("Escape", false).unwrap();
                }
                2 => {
                    key(&mut w, keyboard, 30);
                }
                3 => {
                    key(&mut w, keyboard, 106); // Right collapses selection.
                    w.ui.dispatch(Event::Edit {
                        tab: 1,
                        revision: 0,
                        command: crate::model::Command::Select(crate::model::Selection {
                            anchor: 2,
                            caret: 0,
                        }),
                    })
                    .unwrap();
                }
                _ => w.tick(5000, true).unwrap(),
            }
            drop(writer);
            w.tick(if action == 4 { 5001 } else { 1 }, true).unwrap();
            assert!(w.clipboard.incoming.is_none());
            assert!(!w
                .ui
                .editor()
                .document(1)
                .unwrap()
                .text()
                .contains("do not admit"));
        }
    }

    #[test]
    fn clipboard_offer_retirement_barriers_allow_reuse_without_old_barrier_aliasing() {
        let (mut w, peer, _keyboard, device) = clipboard_fixture();
        let id = 0xff00_0010;
        selection_offer(&mut w, device, id, &[crate::data::PLAIN]);
        w.event(message(device, 5, &[0])).unwrap();
        let old_barrier = *w.clipboard.barriers.keys().next().unwrap();
        w.event(text_event(id, 0, crate::data::UTF8)).unwrap(); // retired event drains
        selection_offer(&mut w, device, id, &[crate::data::PLAIN]);
        w.event(message(old_barrier, 0, &[0])).unwrap();
        w.event(message(DISPLAY, 1, &[old_barrier])).unwrap();
        assert_eq!(
            w.clipboard.offers.get(&id).unwrap().mime(),
            Some(crate::data::PLAIN)
        );
        for _ in 0..150 {
            w.event(message(device, 5, &[0])).unwrap();
            let barrier = *w.clipboard.barriers.keys().next().unwrap();
            w.event(message(barrier, 0, &[0])).unwrap();
            w.event(message(DISPLAY, 1, &[barrier])).unwrap();
            assert!(w.clipboard.offers.is_empty());
            selection_offer(&mut w, device, id, &[crate::data::PLAIN]);
            drain(&peer);
        }
        assert_eq!(w.clipboard.offers.len(), 1);
    }

    #[test]
    fn clipboard_selection_before_keyboard_enter_is_retained_but_cannot_paste_unfocused() {
        let (mut w, peer, keyboard, device) = clipboard_fixture();
        w.event(message(keyboard, 2, &[0, SURFACE])).unwrap();
        selection_offer(&mut w, device, 0xff00_0010, &["text/plain;charset=UTF-8"]);
        w.clipboard_request("paste", 1, 0).unwrap();
        assert!(w.clipboard.incoming.is_none());
        assert!(drain(&peer).0.is_empty());
        assert_eq!(w.clipboard.selection, Some(0xff00_0010));
        assert!(!w.clipboard.offers.get(&0xff00_0010).unwrap().retired);
        focus(&mut w, keyboard);
        w.clipboard_request("paste", 1, 0).unwrap();
        let (requests, files) = drain(&peer);
        assert_eq!(
            requests,
            [text_event(0xff00_0010, 1, "text/plain;charset=UTF-8")]
        );
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn clipboard_rapid_offer_reuse_coalesces_retirements_behind_one_barrier() {
        let (mut w, peer, _keyboard, device) = clipboard_fixture();
        let id = 0xff00_0010;
        for _ in 0..100 {
            selection_offer(&mut w, device, id, &[crate::data::PLAIN]);
            w.event(message(device, 5, &[0])).unwrap();
            assert_eq!(w.clipboard.barriers.len(), 1);
            assert_eq!(w.clipboard.offers.len(), 1);
            drain(&peer); // deliberately withhold callback acknowledgements
        }
        let first = *w.clipboard.barriers.keys().next().unwrap();
        w.event(message(first, 0, &[0])).unwrap();
        w.event(message(DISPLAY, 1, &[first])).unwrap();
        assert_eq!(w.clipboard.offers.len(), 1); // first generation cannot remove last
        let last = *w.clipboard.barriers.keys().next().unwrap();
        w.event(message(last, 0, &[0])).unwrap();
        w.event(message(DISPLAY, 1, &[last])).unwrap();
        assert!(w.clipboard.offers.is_empty());
        assert!(w.clipboard.barriers.is_empty());
    }

    #[test]
    fn clipboard_drag_offer_is_never_the_selection_and_malformed_sends_keep_fd_fifo() {
        let (mut w, _peer, _keyboard, device) = clipboard_fixture();
        selection_offer(&mut w, device, 0xff00_0010, &[crate::data::PLAIN]);
        w.event(message(device, 0, &[0xff00_0011])).unwrap();
        w.event(text_event(0xff00_0011, 0, crate::data::UTF8))
            .unwrap();
        w.event(message(device, 1, &[0, SURFACE, 0, 0, 0xff00_0011]))
            .unwrap();
        assert_eq!(w.clipboard.selection, Some(0xff00_0010));
        assert!(w.clipboard.offers.get(&0xff00_0011).unwrap().retired);
        let source = w.allocate(Kind::RetiredClipboardSource).unwrap();
        let (_reader, writer) = std::io::pipe().unwrap();
        w.connection.descriptors.push_back(writer.into());
        let mut malformed = text_event(source, 1, crate::data::PLAIN);
        malformed.payload.extend_from_slice(&[0; 4]);
        assert!(w.message_needs_descriptor(&malformed).is_err());
        assert!(w.event(malformed).is_err());
        assert_eq!(w.connection.descriptors.len(), 1);
        w.event(text_event(source, 1, crate::data::PLAIN)).unwrap();
        assert!(w.connection.descriptors.is_empty());
        assert!(w.clipboard.outgoing.is_none());
        w.event(message(DISPLAY, 1, &[source])).unwrap();
    }

    #[test]
    fn clipboard_busy_unsupported_and_retired_sends_close_their_own_rights() {
        let (mut w, peer, keyboard, _device) = clipboard_fixture();
        w.event(message(keyboard, 4, &[0, 4, 0, 0, 0])).unwrap();
        key(&mut w, keyboard, 46);
        let source = w.clipboard.source.as_ref().unwrap().0;
        drain(&peer);
        let (mut first_reader, first_writer) = UnixStream::pair().unwrap();
        first_reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let first_writer = File::from(OwnedFd::from(first_writer));
        source_send(&mut w, &peer, source, crate::data::PLAIN, &first_writer);
        drop(first_writer);
        assert!(w.clipboard.outgoing.is_some());
        // Busy Copy does not replace the retained source snapshot.
        key(&mut w, keyboard, 46);
        assert_eq!(w.clipboard.source.as_ref().unwrap().0, source);
        assert!(drain(&peer).0.is_empty());
        for (index, mime) in [crate::data::UTF8, "image/png", crate::data::PLAIN]
            .into_iter()
            .enumerate()
        {
            if index == 2 {
                w.event(message(source, 2, &[])).unwrap();
            }
            let (mut reader, writer) = UnixStream::pair().unwrap();
            reader
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let writer = File::from(OwnedFd::from(writer));
            source_send(&mut w, &peer, source, mime, &writer);
            drop(writer);
            assert_eq!(reader.read(&mut [0]).unwrap(), 0);
            assert!(w.connection.descriptors.is_empty());
        }
        assert!(w.clipboard.source.is_none());
        w.tick(1, true).unwrap();
        let mut text = String::new();
        first_reader.read_to_string(&mut text).unwrap();
        assert_eq!(text, "é");
        w.event(message(DISPLAY, 1, &[source])).unwrap();
    }

    #[test]
    fn clipboard_deadlines_expire_even_while_protocol_queue_defers_admission() {
        let (mut w, peer, _keyboard, device) = clipboard_fixture();
        selection_offer(&mut w, device, 0xff00_0010, &[crate::data::UTF8]);
        w.clipboard_request("paste", 1, 0).unwrap();
        let (_, mut files) = drain(&peer);
        let mut writer = files.pop().unwrap();
        writer.write_all(b"complete but deferred").unwrap();
        drop(writer);
        w.tick(4999, false).unwrap();
        assert!(w.clipboard.incoming.is_some());
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "é abc\n");
        w.tick(5000, false).unwrap();
        assert!(w.clipboard.incoming.is_none());
        w.event(message(WM, 0, &[77])).unwrap();
        assert!(drain(&peer).0.contains(&message(WM, 3, &[77])));
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "é abc\n");
    }

    #[test]
    fn clipboard_offer_budgets_unknown_ids_and_retired_device_events_are_bounded() {
        let (mut w, peer, _keyboard, device) = clipboard_fixture();
        assert!(w.event(message(device, 0, &[31])).is_err());
        assert!(w.event(message(device, 5, &[0xff00_0000])).is_err());
        w.event(message(device, 0, &[0xff00_0000])).unwrap();
        assert!(w.event(message(device, 0, &[0xff00_0000])).is_err());
        for _ in 0..64 {
            w.event(text_event(0xff00_0000, 0, "image/png")).unwrap();
        }
        w.event(text_event(0xff00_0000, 0, crate::data::UTF8))
            .unwrap();
        assert!(w
            .clipboard
            .offers
            .get(&0xff00_0000)
            .unwrap()
            .mime()
            .is_none());
        for id in 1..32 {
            w.event(message(device, 0, &[0xff00_0000 + id])).unwrap();
        }
        assert!(w.event(message(device, 0, &[0xff00_0100])).is_err());
        w.release_clipboard().unwrap();
        drain(&peer);
        let barriers: Vec<_> = w.clipboard.barriers.keys().copied().collect();
        for barrier in barriers {
            w.event(message(barrier, 0, &[0])).unwrap();
            w.event(message(DISPLAY, 1, &[barrier])).unwrap();
        }
        w.event(message(device, 0, &[0xff00_0100])).unwrap();
        w.event(text_event(0xff00_0100, 0, crate::data::PLAIN))
            .unwrap();
        w.event(message(device, 5, &[0xff00_0100])).unwrap();
        assert!(w.clipboard.offers.get(&0xff00_0100).unwrap().retired);
        assert!(w.clipboard.selection.is_none());
        assert_eq!(w.kind(device).unwrap(), Kind::RetiredClipboardDevice);
        w.event(message(DISPLAY, 1, &[device])).unwrap();
    }

    #[test]
    fn clipboard_empty_or_oversized_copy_preserves_the_existing_source() {
        let (mut w, peer, keyboard, _) = clipboard_fixture();
        w.event(message(keyboard, 4, &[0, 4, 0, 0, 0])).unwrap();
        key(&mut w, keyboard, 46);
        let source = w.clipboard.source.as_ref().unwrap().0;
        drain(&peer);
        w.ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: crate::model::Command::Select(crate::model::Selection::default()),
        })
        .unwrap();
        key(&mut w, keyboard, 46);
        assert!(w.notice.as_ref().unwrap().contains("Nothing selected"));
        assert_eq!(w.clipboard.source.as_ref().unwrap().0, source);
        let large = vec![b'x'; crate::clipboard::MAX_BYTES + 1];
        w.ui.dispatch(Event::Load(&large)).unwrap();
        w.ui.dispatch(Event::Edit {
            tab: 2,
            revision: 0,
            command: crate::model::Command::Select(crate::model::Selection {
                anchor: 0,
                caret: large.len(),
            }),
        })
        .unwrap();
        key(&mut w, keyboard, 46);
        assert!(w.notice.as_ref().unwrap().contains("Copy refused: limit"));
        assert_eq!(w.clipboard.source.as_ref().unwrap().0, source);
        assert_eq!(&*w.clipboard.source.as_ref().unwrap().1, "é");
        assert!(drain(&peer).0.is_empty());
        assert_eq!(w.ui.editor().document(2).unwrap().text().as_bytes(), large);
    }

    #[test]
    fn clipboard_empty_eof_reports_ignored_and_preserves_selection() {
        let (mut w, peer, _, device) = clipboard_fixture();
        let selected = w.ui.editor().document(1).unwrap().selection();
        selection_offer(&mut w, device, 0xff00_0010, &[crate::data::UTF8]);
        w.clipboard_request("paste", 1, 0).unwrap();
        let (_, files) = drain(&peer);
        drop(files);
        w.tick(1, true).unwrap();
        assert!(w
            .notice
            .as_ref()
            .unwrap()
            .contains("empty; selection retained"));
        assert_eq!(w.ui.editor().document(1).unwrap().selection(), selected);
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "é abc\n");
        assert_eq!(w.ui.editor().document(1).unwrap().history_depth(), (0, 0));
    }

    #[test]
    fn clipboard_refuses_expired_drag_and_long_or_excessive_mimes_without_disconnecting() {
        let (mut w, peer, keyboard, device) = clipboard_fixture();
        w.event(message(device, 0, &[0xff00_0010])).unwrap();
        w.event(text_event(0xff00_0010, 0, &"x".repeat(257)))
            .unwrap();
        w.event(text_event(0xff00_0010, 0, crate::data::UTF8))
            .unwrap();
        assert_eq!(
            w.clipboard.offers.get(&0xff00_0010).unwrap().mime(),
            Some(crate::data::UTF8)
        );
        for _ in 0..100 {
            w.event(text_event(0xff00_0010, 0, "image/png")).unwrap();
        }
        assert_eq!(w.clipboard.offers.get(&0xff00_0010).unwrap().count, 64);
        w.event(message(keyboard, 2, &[0, SURFACE])).unwrap();
        let barrier = *w.clipboard.barriers.keys().next().unwrap();
        w.event(message(barrier, 0, &[0])).unwrap();
        w.event(message(DISPLAY, 1, &[barrier])).unwrap();
        assert!(w.clipboard.offers.is_empty());
        w.event(message(device, 1, &[0, SURFACE, 0, 0, 0xff00_0010]))
            .unwrap();
        assert!(!w.closed);
        focus(&mut w, keyboard);
        w.event(message(keyboard, 4, &[0, 4, 0, 0, 0])).unwrap();
        key(&mut w, keyboard, 46);
        let source = w.clipboard.source.as_ref().unwrap().0;
        drain(&peer);
        let (mut reader, writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let writer = File::from(OwnedFd::from(writer));
        source_send(&mut w, &peer, source, &"x".repeat(257), &writer);
        drop(writer);
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
        assert!(!w.closed);
        assert!(w.connection.descriptors.is_empty());
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "é abc\n");
    }

    #[test]
    fn actual_descriptor_and_keyboard_events_edit_both_profiles_and_pixels() {
        for profile in [Profile::Windows, Profile::Emacs] {
            let (mut w, peer, device) = seat_fixture();
            w.ui.dispatch(Event::Profile(profile)).unwrap();
            let file = map_file();
            send_map(&mut w, &peer, device, &file);
            focus(&mut w, device);
            key(&mut w, device, 1); // dismiss notice
            w.event(message(SHM, 0, &[1])).unwrap();
            configure(&mut w, 800, 600);
            w.draw().unwrap();
            let before = w.pixels.clone();
            drain(&peer);
            let tab = w.ui.editor().active().unwrap();
            let original = w.ui.editor().document(tab).unwrap().text().to_owned();
            key(&mut w, device, 30);
            let text = w.ui.editor().document(tab).unwrap().text();
            assert_eq!(text, format!("a{original}"));
            assert!(w.ui.editor().document(tab).unwrap().dirty());
            done(&mut w);
            w.draw().unwrap();
            assert_ne!(w.pixels, before);
            drain(&peer);
            w.event(message(device, 4, &[0, 4, 0, 0, 0])).unwrap(); // Control
            key(
                &mut w,
                device,
                if profile == Profile::Windows { 44 } else { 53 },
            );
            assert_eq!(w.ui.editor().document(tab).unwrap().text(), original);
            assert!(!w.ui.editor().document(tab).unwrap().dirty());
            key(&mut w, device, 15); // Ctrl+Tab
            assert_ne!(w.ui.editor().active().unwrap(), tab);
        }
    }

    #[test]
    fn file_prompts_save_snapshots_and_keep_window_alive_during_io() {
        for profile in [Profile::Windows, Profile::Emacs] {
            let (mut w, peer, device) = seat_fixture();
            send_map(&mut w, &peer, device, &map_file());
            focus(&mut w, device);
            w.ui = Controller::default();
            w.ui.dispatch(Event::New).unwrap();
            w.ui.dispatch(Event::Profile(profile)).unwrap();
            w.labels.clear();
            w.files = Some(crate::session::Session::start().unwrap());
            let tab = w.ui.editor().active().unwrap();
            key(&mut w, device, 30); // real descriptor/map/seat path inserts 'a'
            let text = w.ui.editor().document(tab).unwrap().text().to_owned();
            let save = |w: &mut Window| {
                if profile == Profile::Emacs {
                    w.chord("C-x", false).unwrap();
                }
                w.chord("C-s", false).unwrap();
            };
            save(&mut w);
            assert!(w.prompt.is_some());
            w.chord("b", false).unwrap();
            w.chord("Return", true).unwrap(); // held confirmations never submit
            assert!(!w.files.as_ref().unwrap().busy());
            w.chord("C-g", false).unwrap();
            assert!(w.prompt.is_none());
            assert_eq!(w.ui.editor().document(tab).unwrap().text(), text);
            save(&mut w);
            let directory = std::env::temp_dir().join(format!(
                "td-editor-window-{}-{}",
                std::process::id(),
                NEXT_FILE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join("saved draft");
            for c in path.to_str().unwrap().chars() {
                w.chord(&c.to_string(), false).unwrap();
            }
            assert!(w.prompt.as_ref().unwrap().notice().contains("saved draft"));
            w.event(message(SHM, 0, &[1])).unwrap();
            configure(&mut w, 800, 600);
            w.draw().unwrap();
            let prompt_pixels = w.pixels.clone();
            drain(&peer);
            w.chord("Return", false).unwrap();
            assert!(w.files.as_ref().unwrap().busy());
            w.close();
            assert!(!w.closed && !w.quitting);
            w.chord(
                if profile == Profile::Emacs {
                    "C-x"
                } else {
                    "C-w"
                },
                false,
            )
            .unwrap();
            if profile == Profile::Emacs {
                w.chord("k", false).unwrap();
            }
            assert!(w.ui.editor().document(tab).is_ok());
            assert_eq!(
                w.notice.as_deref(),
                Some("File operation pending; wait before closing tabs.")
            );
            w.event(message(device, 3, &[0, 0, 48, 1])).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while w.files.as_ref().unwrap().busy() {
                assert!(Instant::now() < deadline, "file worker completion timeout");
                w.tick(w.clock + 1, false).unwrap();
                std::thread::yield_now();
            }
            assert_eq!(std::fs::read(&path).unwrap(), b"a");
            assert_eq!(w.ui.editor().document(tab).unwrap().text(), "ab");
            assert!(w.input.repeat(w.clock + 1000).unwrap().is_some());
            assert!(w.ui.editor().document(tab).unwrap().dirty());
            assert!(w
                .notice
                .as_ref()
                .unwrap()
                .contains("newer edits remain unsaved"));
            done(&mut w);
            w.draw().unwrap();
            assert_ne!(w.pixels, prompt_pixels);
            drain(&peer);
            w.close();
            assert!(w.closing.is_some() && !w.closed);
            assert!(w
                .closing_notice()
                .unwrap()
                .contains("completed saves stay saved"));
            w.chord("C-d", true).unwrap();
            assert!(!w.closed);
            w.chord("Escape", false).unwrap();
            assert!(w.closing.is_none());
            w.close();
            w.chord("C-d", false).unwrap();
            assert!(w.closed);
            assert_eq!(std::fs::read(&path).unwrap(), b"a");
            std::fs::remove_file(path).unwrap();
            std::fs::remove_dir(directory).unwrap();
        }
    }

    fn file_dialog_fixture() -> (Window, UnixStream) {
        let (mut w, peer, device) = seat_fixture();
        send_map(&mut w, &peer, device, &map_file());
        focus(&mut w, device);
        w.ui = Controller::default();
        w.ui.dispatch(Event::New).unwrap();
        w.labels.clear();
        w.files = Some(crate::session::Session::start().unwrap());
        (w, peer)
    }

    #[test]
    fn native_read_only_state_names_modal_state_without_mutating_it() {
        let (mut w, _peer) = file_dialog_fixture();
        let query = crate::control::Request {
            id: 77,
            operation: crate::control::Operation::State,
        };
        let before = query.response(&w.ui);
        let generation = w.frames.generation().unwrap();
        let response = w.control_response(&query);
        assert_eq!(
            response,
            format!(
                "{before}\tadapter=native-paths\tnative=0,1,0,0\tmodal=0,0,0,0,0,0,0,0,0\tspelling=-,0\tjob-last=0\tdialog-last=0\tdialog=-\twindow-generation={generation}\tframe-submitted=-\tframe-completed=-"
            )
        );
        assert_eq!(query.response(&w.ui), before);
        w.chord("C-f", false).unwrap();
        assert!(w.search.is_some());
        let before = query.response(&w.ui);
        let response = w.control_response(&query);
        assert!(response.contains("\tmodal=0,0,0,0,0,1,0,0,0\t"));
        assert!(w.search.is_some());
        assert_eq!(query.response(&w.ui), before);
        w.chord("Escape", false).unwrap();
        assert!(w.search.is_none());
        let refused = crate::control::Request::parse(b"1\t77\tnew\textra")
            .unwrap_err()
            .response();
        assert_eq!(w.native_state(refused.clone()), Ok(refused));
    }

    #[test]
    fn native_read_only_text_preserves_revision_checks_across_edit_and_undo() {
        let (mut w, _peer) = file_dialog_fixture();
        w.chord("a", false).unwrap();
        let tab = w.ui.editor().active().unwrap();
        let revision = w.ui.editor().document(tab).unwrap().revision();
        let query = crate::control::Request {
            id: 78,
            operation: crate::control::Operation::Text {
                tab,
                revision,
                offset: 0,
                limit: 4,
            },
        };
        assert_eq!(w.control_response(&query), query.response(&w.ui));
        assert!(w.control_response(&query).ends_with("\t61"));
        w.chord("b", false).unwrap();
        assert!(w
            .control_response(&query)
            .contains("\terror\tstale-revision\t"));
        w.chord("C-z", false).unwrap();
        assert!(w
            .control_response(&query)
            .contains("\terror\tstale-revision\t"));
    }

    fn control_client(path: &Path, payload: &[u8]) -> UnixStream {
        let mut client = UnixStream::connect(path).unwrap();
        client
            .write_all(&crate::control::frame(payload).unwrap())
            .unwrap();
        client.set_nonblocking(true).unwrap();
        client
    }

    fn control_answer(w: &mut Window, client: &mut UnixStream, peer: &UnixStream) -> String {
        control_answer_pump(w, client, peer, true)
    }

    fn control_answer_pump(
        w: &mut Window,
        client: &mut UnixStream,
        peer: &UnixStream,
        advance_turn: bool,
    ) -> String {
        drain(peer);
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut decoder = crate::control::Decoder::default();
        let mut bytes = [0; 4096];
        loop {
            assert!(Instant::now() < deadline, "native control response timeout");
            if advance_turn {
                w.end_turn(w.clock, false).unwrap();
            } else {
                w.control_tick();
            }
            match client.read(&mut bytes) {
                Ok(0) => return String::from_utf8(decoder.finish().unwrap()).unwrap(),
                Ok(count) => decoder.push(&bytes[..count]).unwrap(),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("native control read: {error}"),
            }
        }
    }

    fn job_request(
        w: &mut Window,
        peer: &UnixStream,
        path: &std::path::Path,
        payload: &str,
    ) -> String {
        let mut client = control_client(path, payload.as_bytes());
        control_answer_pump(w, &mut client, peer, false) // Explicitly withhold background steps.
    }

    #[test]
    fn remote_save_jobs_recheck_queued_revisions_and_report_exact_snapshot_outcomes() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path("control");
        let file = directory.path("draft");
        let destination = directory
            .0
            .join(std::ffi::OsString::from_vec(b"saved-\xff".to_vec()));
        std::fs::write(&file, b"disk").unwrap();
        let (mut w, peer) = file_dialog_fixture();
        w.files.as_mut().unwrap().open(file.clone()).unwrap();
        finish_file(&mut w);
        w.chord("a", false).unwrap();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&socket).unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(
            job_request(&mut w, &peer, &socket, "1\t1\tsave\t2\t1"),
            "1\t1\tpending\t1"
        );
        let before = job_request(&mut w, &peer, &socket, "1\t0\tstate");
        assert!(before.contains("job=1,save,2,1,0,pending,-"));
        for command in [
            "save\t2\t1",
            "save-as\t2\t1\t61",
            "open\t61",
            "quit",
            "close-tab\t2\t1",
        ] {
            assert!(
                job_request(&mut w, &peer, &socket, &format!("1\t2\t{command}"))
                    .contains("\terror\tunavailable\t")
            );
            assert_eq!(job_request(&mut w, &peer, &socket, "1\t0\tstate"), before);
        }
        assert_eq!(
            job_request(&mut w, &peer, &socket, "1\t3\tinsert\t2\t1\t1\t1\t62"),
            "1\t3\tok\t"
        );
        assert_eq!(
            job_request(&mut w, &peer, &socket, "1\t4\tundo\t2\t2"),
            "1\t4\tok\t"
        );
        finish_file(&mut w);
        assert_eq!(std::fs::read(&file).unwrap(), b"disk");
        assert_eq!(w.ui.editor().document(2).unwrap().text(), "adisk");
        assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
            .contains("job=1,save,2,1,0,error,stale-revision"));
        assert_eq!(
            job_request(&mut w, &peer, &socket, "1\t5\tsave\t2\t3"),
            "1\t5\tpending\t2"
        );
        w.tick(w.clock + 1, false).unwrap(); // Exact ordinary worker handoff.
        assert!(w.files.as_ref().unwrap().busy());
        w.chord("c", false).unwrap();
        finish_file(&mut w);
        assert_eq!(std::fs::read(&file).unwrap(), b"adisk");
        assert_eq!(w.ui.editor().document(2).unwrap().text(), "acdisk");
        assert!(w.ui.editor().document(2).unwrap().dirty());
        assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
            .contains("job=2,save,2,3,0,complete,-"));
        let save_as = format!(
            "1\t6\tsave-as\t2\t4\t{}",
            crate::control::hex(destination.as_os_str().as_bytes())
        );
        assert_eq!(
            job_request(&mut w, &peer, &socket, &save_as),
            "1\t6\tpending\t3"
        );
        w.chord("C-n", false).unwrap();
        finish_file(&mut w);
        assert_eq!(w.ui.editor().active(), Some(3));
        assert_eq!(std::fs::read(&destination).unwrap(), b"acdisk");
        assert!(!w.ui.editor().document(2).unwrap().dirty());
        assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
            .contains("job=3,save-as,2,4,0,complete,-"));
        assert_eq!(
            job_request(&mut w, &peer, &socket, "1\t7\tselect-tab\t2\t4"),
            "1\t7\tok\t"
        );
        w.chord("d", false).unwrap();
        std::fs::write(&destination, b"external").unwrap();
        assert_eq!(
            job_request(&mut w, &peer, &socket, "1\t8\tsave\t2\t5"),
            "1\t8\tpending\t4"
        );
        finish_file(&mut w);
        assert!(w.conflict.is_some());
        assert!(w.ui.editor().document(2).unwrap().dirty());
        assert_eq!(std::fs::read(&destination).unwrap(), b"external");
        assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
            .contains("job=4,save,2,5,0,error,unavailable"));
        w.chord("Escape", false).unwrap();
        let existing = format!(
            "1\t9\tsave-as\t2\t5\t{}",
            crate::control::hex(file.as_os_str().as_bytes())
        );
        assert_eq!(
            job_request(&mut w, &peer, &socket, &existing),
            "1\t9\tpending\t5"
        );
        finish_file(&mut w);
        assert_eq!(std::fs::read(file).unwrap(), b"adisk");
        assert_eq!(std::fs::read(destination).unwrap(), b"external");
        assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
            .contains("job=5,save-as,2,5,0,error,unavailable"));
        w.stop_control();
    }

    #[test]
    fn remote_save_refusals_do_not_reserve_jobs_or_change_native_state() {
        let (mut w, _peer) = file_dialog_fixture();
        let state = crate::control::Request::parse(b"1\t0\tstate").unwrap();
        w.ui.dispatch(Event::New).unwrap();
        for (command, code) in [
            ("save\t99\t0", "missing-tab"),
            ("save\t2\t1", "stale-revision"),
            ("save-as\t1\t0\t61", "invalid-argument"),
            ("save\t2\t0", "invalid-argument"),
        ] {
            let before = w.control_response(&state);
            let notice = w.notice.clone();
            let request =
                crate::control::Request::parse(format!("1\t1\t{command}").as_bytes()).unwrap();
            assert!(w
                .control_response(&request)
                .contains(&format!("\terror\t{code}\t")));
            assert_eq!(w.control_response(&state), before);
            assert_eq!(w.notice, notice);
        }
        for guard in ["scratch", "closed", "counter", "jobs"] {
            let (mut w, _peer) = file_dialog_fixture();
            match guard {
                "scratch" => w.files = None,
                "closed" => w.closed = true,
                "counter" => w.ui.generation_for_test(u64::MAX),
                "jobs" => w.control_jobs.exhaust_for_test(),
                _ => panic!("unknown guard"),
            }
            let before = w.control_response(&state);
            let notice = w.notice.clone();
            let request = crate::control::Request::parse(b"1\t2\tsave-as\t1\t0\t61").unwrap();
            let code = if matches!(guard, "counter" | "jobs") {
                "exhausted"
            } else {
                "unavailable"
            };
            assert!(w
                .control_response(&request)
                .contains(&format!("\terror\t{code}\t")));
            assert_eq!(w.control_response(&state), before);
            assert_eq!(w.notice, notice);
        }
        let directory = DialogDirectory::new();
        let path = directory.path("untitled-save-as");
        let (mut w, _peer) = file_dialog_fixture();
        w.chord("a", false).unwrap();
        let request = crate::control::Request::parse(
            format!(
                "1\t3\tsave-as\t1\t1\t{}",
                crate::control::hex(path.as_os_str().as_encoded_bytes())
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(w.control_response(&request), "1\t3\tpending\t1");
        assert!(w.prompt.is_none());
        finish_file(&mut w);
        assert_eq!(std::fs::read(path).unwrap(), b"a");
        assert!(!w.ui.editor().document(1).unwrap().dirty());
        assert!(w.files.as_ref().unwrap().associated(1));
        assert!(w
            .control_response(&state)
            .contains("job=1,save-as,1,1,0,complete,-"));
        for observed in [false, true] {
            let (mut w, _peer) = file_dialog_fixture();
            w.files = Some(crate::session::Session::disconnected_for_test());
            if observed {
                w.tick(w.clock + 1, false).unwrap();
            }
            let request = crate::control::Request::parse(b"1\t4\tsave-as\t1\t0\t61").unwrap();
            assert_eq!(w.control_response(&request), "1\t4\tpending\t1");
            finish_file(&mut w);
            assert!(w
                .control_response(&state)
                .contains("job=1,save-as,1,0,0,error,unavailable"));
            assert!(!w.files.as_ref().unwrap().associated(1));
            assert!(w.control_file_job.is_none());
        }
    }

    #[test]
    fn remote_open_jobs_pin_completed_tabs_across_native_activity_and_file_failures() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path("control");
        let file = directory
            .0
            .join(std::ffi::OsString::from_vec(b"draft-\xff".to_vec()));
        std::fs::write(&file, b"disk").unwrap();
        let open = |path: &Path| {
            format!(
                "1\t1\topen\t{}",
                crate::control::hex(path.as_os_str().as_bytes())
            )
        };
        let (mut w, peer) = file_dialog_fixture();
        w.chord("a", false).unwrap();
        let original = format!("{:?}", w.ui.editor().document(1).unwrap());
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&socket).unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(
            job_request(&mut w, &peer, &socket, &open(&file)),
            "1\t1\tpending\t1"
        );
        assert!(w.files.as_ref().unwrap().busy());
        let before = job_request(&mut w, &peer, &socket, "1\t0\tstate");
        assert!(before.contains("job=1,open,0,0,0,pending,-"));
        for request in [
            open(&file),
            "1\t2\tclose-tab\t1\t1".into(),
            "1\t3\tquit".into(),
        ] {
            assert!(
                job_request(&mut w, &peer, &socket, &request).contains("\terror\tunavailable\t")
            );
            assert_eq!(job_request(&mut w, &peer, &socket, "1\t0\tstate"), before);
        }
        w.chord("C-n", false).unwrap();
        assert_eq!(w.ui.editor().active(), Some(2));
        finish_file(&mut w);
        assert_eq!(w.ui.editor().active(), Some(3));
        assert_eq!(w.ui.editor().document(3).unwrap().text(), "disk");
        w.chord("C-n", false).unwrap();
        assert_eq!(w.ui.editor().active(), Some(4));
        assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
            .contains("job=1,open,3,0,0,complete,-"));
        assert_eq!(
            format!("{:?}", w.ui.editor().document(1).unwrap()),
            original
        );
        assert_eq!(
            job_request(&mut w, &peer, &socket, "1\t5\tselect-tab\t3\t0"),
            "1\t5\tok\t"
        );
        assert_eq!(
            job_request(&mut w, &peer, &socket, "1\t4\tinsert\t3\t0\t0\t0\t78"),
            "1\t4\tok\t"
        );
        assert_eq!(
            job_request(&mut w, &peer, &socket, "1\t6\tselect-tab\t4\t0"),
            "1\t6\tok\t"
        );
        // Duplicate Open selects the already-associated document, including its edits.
        assert_eq!(
            job_request(&mut w, &peer, &socket, &open(&file)),
            "1\t1\tpending\t2"
        );
        finish_file(&mut w);
        assert_eq!(w.ui.editor().active(), Some(3));
        assert_eq!(w.ui.editor().document(3).unwrap().text(), "xdisk");
        assert!(w.ui.editor().document(3).unwrap().dirty());
        assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
            .contains("job=2,open,3,1,0,complete,-"));
        assert_eq!(std::fs::read(&file).unwrap(), b"disk");
        let missing = directory.path("new-file");
        assert_eq!(
            job_request(&mut w, &peer, &socket, &open(&missing)),
            "1\t1\tpending\t3"
        );
        finish_file(&mut w);
        assert_eq!(w.ui.editor().active(), Some(5));
        assert!(w.ui.editor().document(5).unwrap().dirty());
        assert!(!missing.exists());
        assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
            .contains("job=3,open,5,0,0,complete,-"));
        let invalid = directory.path("invalid-text");
        std::fs::write(&invalid, b"\xff").unwrap();
        let documents = format!("{:?}", w.ui.editor());
        assert_eq!(
            job_request(&mut w, &peer, &socket, &open(&invalid)),
            "1\t1\tpending\t4"
        );
        finish_file(&mut w);
        assert_eq!(format!("{:?}", w.ui.editor()), documents);
        let state = job_request(&mut w, &peer, &socket, "1\t0\tstate");
        assert!(state.contains("job=4,open,0,0,0,error,unavailable"));
        assert!(state.contains("job=1,open,3,0,0,complete,-"));
        assert!(!state.contains("draft") && !state.contains("invalid-text"));
        w.stop_control();
    }

    #[test]
    fn remote_open_reports_disconnected_worker_submission_failures_as_terminal_jobs() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        w.files = Some(crate::session::Session::disconnected_for_test());
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        let documents = format!("{:?}", w.ui.editor());
        for id in [1, 2] {
            // First send meets a closed channel; poll then observes the dead worker.
            if id == 2 {
                w.tick(w.clock + 1, false).unwrap();
            }
            assert_eq!(
                job_request(&mut w, &peer, &path, "1\t1\topen\t61"),
                format!("1\t1\tpending\t{id}")
            );
            assert!(!w.files.as_ref().unwrap().busy());
            assert!(w.control_file_job.is_none());
            assert!(job_request(&mut w, &peer, &path, "1\t0\tstate")
                .contains(&format!("job={id},open,0,0,0,error,unavailable")));
            assert_eq!(format!("{:?}", w.ui.editor()), documents);
        }
        w.stop_control();
    }

    #[test]
    fn remote_open_and_spelling_jobs_interleave_without_cross_completion() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let file = directory.path("document");
        std::fs::write(&file, b"disk").unwrap();
        let (mut w, peer) = file_dialog_fixture();
        w.chord("a", false).unwrap();
        w.spelling
            .install(crate::spelling::Dictionary::parse(b"known").unwrap());
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(
            job_request(&mut w, &peer, &path, "1\t1\tcheck-spelling\t1\t1"),
            "1\t1\tpending\t1"
        );
        let request = format!(
            "1\t2\topen\t{}",
            crate::control::hex(file.as_os_str().as_encoded_bytes())
        );
        assert_eq!(
            job_request(&mut w, &peer, &path, &request),
            "1\t2\tpending\t2"
        );
        finish_file(&mut w);
        let state = job_request(&mut w, &peer, &path, "1\t0\tstate");
        assert!(state.contains("job=1,spelling,1,1,1,pending,-"));
        assert!(state.contains("job=2,open,2,0,0,complete,-"));
        w.end_turn(w.clock, false).unwrap();
        let state = job_request(&mut w, &peer, &path, "1\t0\tstate");
        assert!(state.contains("job=1,spelling,1,1,1,complete,-"));
        assert!(state.contains("job=2,open,2,0,0,complete,-"));
        w.stop_control();
    }

    #[test]
    fn remote_open_refusals_and_completion_capacity_failures_preserve_documents() {
        let directory = DialogDirectory::new();
        let file = directory.path("draft");
        std::fs::write(&file, b"disk").unwrap();
        let request = crate::control::Request {
            id: 1,
            operation: crate::control::Operation::Open(file.clone()),
        };
        let state = crate::control::Request::parse(b"1\t0\tstate").unwrap();
        for guard in ["scratch", "counter", "jobs", "closed"] {
            let (mut w, _peer) = file_dialog_fixture();
            match guard {
                "scratch" => w.files = None,
                "counter" => w.ui.generation_for_test(u64::MAX),
                "jobs" => w.control_jobs.exhaust_for_test(),
                "closed" => w.closed = true,
                _ => panic!("unknown guard"),
            }
            let before = w.control_response(&state);
            let notice = w.notice.clone();
            assert!(w.control_response(&request).contains(
                if matches!(guard, "counter" | "jobs") {
                    "\terror\texhausted\t"
                } else {
                    "\terror\tunavailable\t"
                }
            ));
            assert_eq!(w.control_response(&state), before);
            assert_eq!(w.notice, notice);
        }
        let (mut w, _peer) = file_dialog_fixture();
        for _ in 1..64 {
            w.ui.dispatch(Event::New).unwrap();
        }
        let documents = format!("{:?}", w.ui.editor());
        assert_eq!(w.control_response(&request), "1\t1\tpending\t1");
        finish_file(&mut w);
        assert_eq!(format!("{:?}", w.ui.editor()), documents);
        assert!(w
            .control_response(&state)
            .contains("job=1,open,0,0,0,error,unavailable"));
        assert_eq!(w.files.as_ref().unwrap().labels().count(), 0);
        assert_eq!(std::fs::read(file).unwrap(), b"disk");
    }

    #[test]
    fn remote_path_answers_bind_reopened_prompts_and_keep_partial_entries_on_refusal() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path("control");
        let file = directory
            .0
            .join(std::ffi::OsString::from_vec(b"source-\xff".to_vec()));
        std::fs::write(&file, b"opened").unwrap();
        let (mut w, peer) = file_dialog_fixture();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&socket).unwrap(),
            )
            .unwrap(),
        );
        w.chord("C-o", false).unwrap();
        assert!(w
            .control_dialog_fields()
            .ends_with("1,path-open,path,1,0,cancel+path"));
        w.path_chord("s", false);
        assert_eq!(
            job_request(
                &mut w,
                &peer,
                &socket,
                "1\t1\tdialog-answer\t1\t1\t0\tcancel"
            ),
            "1\t1\tok\t"
        );
        w.chord("C-o", false).unwrap();
        w.path_chord("s", false);
        configure(&mut w, 100, 80);
        let device = w.device.unwrap();
        w.event(message(device, 2, &[99, SURFACE])).unwrap();
        let before = job_request(&mut w, &peer, &socket, "1\t0\tstate");
        for (args, code) in [
            ("1\t1\t0\tpath\t61", "invalid-argument"),
            ("2\t2\t0\tpath\t61", "invalid-argument"),
            ("2\t1\t1\tpath\t61", "stale-revision"),
            ("2\t1\t0\tsave", "unavailable"),
        ] {
            assert!(job_request(
                &mut w,
                &peer,
                &socket,
                &format!("1\t2\tdialog-answer\t{args}")
            )
            .contains(&format!("\terror\t{code}\t")));
            assert_eq!(job_request(&mut w, &peer, &socket, "1\t0\tstate"), before);
            assert_eq!(w.prompt.as_ref().unwrap().text, "s");
        }
        let request = format!(
            "1\t3\tdialog-answer\t2\t1\t0\tpath\t{}",
            crate::control::hex(file.as_os_str().as_encoded_bytes())
        );
        assert_eq!(
            job_request(&mut w, &peer, &socket, &request),
            "1\t3\tpending\t1"
        );
        assert!(w.prompt.is_none());
        finish_file(&mut w);
        assert_eq!(w.ui.editor().document(2).unwrap().text(), "opened");
        assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
            .contains("job=1,open,2,0,0,complete,-"));
        w.stop_control();
    }

    #[test]
    fn remote_save_as_path_keeps_the_native_conflict_target_inactive() {
        let directory = DialogDirectory::new();
        let (mut w, peer, socket, file) = remote_conflict_fixture(&directory, true);
        configure(&mut w, 800, 600);
        w.conflict_chord("C-s", false);
        assert!(w
            .control_dialog_fields()
            .ends_with("2,path-save-as,path,2,1,cancel+path"));
        assert_eq!(w.ui.editor().active(), Some(3));
        let destination = directory.path("copy");
        let request = format!(
            "1\t2\tdialog-answer\t2\t2\t1\tpath\t{}",
            crate::control::hex(destination.as_os_str().as_encoded_bytes())
        );
        assert_eq!(
            job_request(&mut w, &peer, &socket, &request),
            "1\t2\tpending\t2"
        );
        finish_file(&mut w);
        assert_eq!(std::fs::read(destination).unwrap(), b"adisk");
        assert_eq!(std::fs::read(file).unwrap(), b"external");
        assert_eq!(w.ui.editor().active(), Some(3));
        assert!(!w.ui.editor().document(2).unwrap().dirty());
        w.stop_control();
    }

    #[test]
    fn remote_dictionary_paths_report_global_jobs_and_preserve_failed_replacements() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path("control");
        let dictionary = directory.path("dictionary");
        let (mut w, peer) = file_dialog_fixture();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&socket).unwrap(),
            )
            .unwrap(),
        );
        for (id, bytes, expected) in [
            (1, b"first\n".as_slice(), "complete,-"),
            (2, b"\xff".as_slice(), "error,unavailable"),
            (3, b"second\nthird\n".as_slice(), "complete,-"),
        ] {
            std::fs::write(&dictionary, bytes).unwrap();
            assert!(w.file_request("dictionary", 1, 0));
            assert!(w
                .control_dialog_fields()
                .ends_with(&format!("{id},path-dictionary,path,1,0,cancel+path")));
            let command = format!(
                "1\t1\tdialog-answer\t{id}\t1\t0\tpath\t{}",
                crate::control::hex(dictionary.as_os_str().as_encoded_bytes())
            );
            assert_eq!(
                job_request(&mut w, &peer, &socket, &command),
                format!("1\t1\tpending\t{id}")
            );
            assert!(w.prompt.is_none());
            finish_file(&mut w);
            let state = job_request(&mut w, &peer, &socket, "1\t0\tstate");
            assert!(state.contains(&format!("job={id},dictionary,0,0,0,{expected}")));
            assert!(state.contains(if id == 3 {
                "\tspelling=2,0\t"
            } else {
                "\tspelling=1,0\t"
            }));
            assert_eq!(w.ui.editor().document(1).unwrap().text(), "");
            assert_eq!(w.ui.editor().document(1).unwrap().revision(), 0);
        }
        w.chord("x", false).unwrap();
        assert_eq!(
            job_request(&mut w, &peer, &socket, "1\t2\tcheck-spelling\t1\t1"),
            "1\t2\tpending\t4"
        );
        assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
            .contains("job=4,spelling,1,1,1,pending,-"));
        std::fs::write(&dictionary, b"last\n").unwrap();
        assert!(w.file_request("dictionary", 1, 1));
        let command = format!(
            "1\t3\tdialog-answer\t4\t1\t1\tpath\t{}",
            crate::control::hex(dictionary.as_os_str().as_encoded_bytes())
        );
        assert_eq!(
            job_request(&mut w, &peer, &socket, &command),
            "1\t3\tpending\t5"
        );
        w.ui.dispatch(Event::New).unwrap();
        finish_file(&mut w);
        w.end_turn(w.clock, false).unwrap();
        let state = job_request(&mut w, &peer, &socket, "1\t0\tstate");
        assert!(state.contains("job=4,spelling,1,1,1,cancelled,-"));
        assert!(state.contains("job=5,dictionary,0,0,0,complete,-"));
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "x");
        assert_eq!(w.ui.editor().document(1).unwrap().revision(), 1);
        assert_eq!(w.ui.editor().active(), Some(2));
        w.stop_control();
    }

    #[test]
    fn remote_path_points_reject_stale_missing_and_foreign_editor_targets() {
        for guard in ["stale", "missing", "owner"] {
            let (mut w, _peer) = file_dialog_fixture();
            assert!(w.file_request("open", 1, 0));
            match guard {
                "stale" => {
                    w.ui.dispatch(Event::Edit {
                        tab: 1,
                        revision: 0,
                        command: crate::model::Command::Insert("x".into()),
                    })
                    .unwrap();
                }
                "missing" => {
                    w.ui.dispatch(Event::Close {
                        tab: 1,
                        revision: 0,
                    })
                    .unwrap();
                }
                "owner" => {
                    w.ui = Controller::default();
                    w.ui.dispatch(Event::New).unwrap();
                }
                _ => panic!("unknown fixture"),
            }
            let state = crate::control::Request::parse(b"1\t0\tstate").unwrap();
            let before = w.control_response(&state);
            assert!(before.contains("1,path-open,invalid,-,-,-"));
            for answer in ["cancel", "path\t61"] {
                let request = crate::control::Request::parse(
                    format!("1\t1\tdialog-answer\t1\t1\t0\t{answer}").as_bytes(),
                )
                .unwrap();
                let error = match guard {
                    "stale" => "stale-revision",
                    "missing" => "missing-tab",
                    _ => "invalid-argument",
                };
                assert!(w
                    .control_response(&request)
                    .contains(&format!("\terror\t{error}\t")));
                assert_eq!(w.control_response(&state), before);
                assert!(!w.files.as_ref().unwrap().busy());
            }
        }
    }

    #[test]
    fn remote_path_worker_startup_failures_drop_prompt_with_terminal_job() {
        for (action, kind) in [
            ("open", "open"),
            ("save-as", "save-as"),
            ("dictionary", "dictionary"),
        ] {
            let (mut w, _peer) = file_dialog_fixture();
            assert!(w.file_request(action, 1, 0));
            w.files = Some(crate::session::Session::disconnected_for_test());
            w.tick(w.clock, false).unwrap();
            let request =
                crate::control::Request::parse(b"1\t1\tdialog-answer\t1\t1\t0\tpath\t61").unwrap();
            assert_eq!(w.control_response(&request), "1\t1\tpending\t1");
            assert!(w.prompt.is_none() && w.control_file_job.is_none());
            let state =
                w.control_response(&crate::control::Request::parse(b"1\t0\tstate").unwrap());
            assert!(state.contains(&format!(
                "job=1,{kind},{},0,0,error,unavailable",
                if action == "save-as" { 1 } else { 0 }
            )));
            assert_eq!(w.ui.editor().document(1).unwrap().text(), "");
        }
    }

    #[test]
    fn remote_path_counter_refusals_preserve_identity_entry_and_history() {
        for action in ["open", "save-as", "dictionary"] {
            for guard in ["generation", "jobs"] {
                let (mut w, _peer) = file_dialog_fixture();
                assert!(w.file_request(action, 1, 0));
                w.path_chord("s", false);
                if guard == "generation" {
                    w.ui.generation_for_test(u64::MAX);
                } else {
                    w.control_jobs.exhaust_for_test();
                }
                let state = crate::control::Request::parse(b"1\t0\tstate").unwrap();
                let before = w.control_response(&state);
                let request =
                    crate::control::Request::parse(b"1\t1\tdialog-answer\t1\t1\t0\tpath\t61")
                        .unwrap();
                assert!(w
                    .control_response(&request)
                    .contains("\terror\texhausted\t"));
                assert_eq!(w.control_response(&state), before);
                assert_eq!(w.prompt.as_ref().unwrap().text, "s");
                assert!(!w.files.as_ref().unwrap().busy());
            }
            let (mut w, _peer) = file_dialog_fixture();
            w.last_dialog_id = u64::MAX;
            assert!(!w.file_request(action, 1, 0));
            assert!(w.prompt.is_none());
            assert_eq!(w.last_dialog_id, u64::MAX);
            assert!(w.notice.as_ref().unwrap().contains("exhausted"));
            assert_eq!(w.ui.editor().document(1).unwrap().text(), "");
        }
    }

    fn remote_conflict_fixture(
        directory: &DialogDirectory,
        inactive: bool,
    ) -> (Window, UnixStream, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path("control");
        let file = directory.path("draft");
        std::fs::write(&file, b"disk").unwrap();
        let (mut w, peer) = file_dialog_fixture();
        w.files.as_mut().unwrap().open(file.clone()).unwrap();
        finish_file(&mut w);
        w.chord("a", false).unwrap();
        std::fs::write(&file, b"external").unwrap();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&socket).unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(
            job_request(&mut w, &peer, &socket, "1\t1\tsave\t2\t1"),
            "1\t1\tpending\t1"
        );
        if inactive {
            w.ui.dispatch(Event::New).unwrap();
        }
        finish_file(&mut w);
        assert!(w
            .control_dialog_fields()
            .contains("dialog=1,conflict,question,2,1,cancel+reload+save-as"));
        (w, peer, socket, file)
    }

    #[test]
    fn remote_conflict_reload_needs_live_second_discard_and_keeps_inactive_target() {
        let directory = DialogDirectory::new();
        let (mut w, peer, socket, file) = remote_conflict_fixture(&directory, true);
        configure(&mut w, 100, 80);
        let device = w.device.unwrap();
        w.event(message(device, 2, &[99, SURFACE])).unwrap();
        w.conflict_chord("C-r", false);
        assert!(!w.conflict.as_ref().unwrap().needs_discard());
        let before = job_request(&mut w, &peer, &socket, "1\t0\tstate");
        for (args, code) in [
            ("0\t2\t1\treload", "invalid-argument"),
            ("2\t2\t1\treload", "invalid-argument"),
            ("1\t3\t1\treload", "invalid-argument"),
            ("1\t2\t0\treload", "stale-revision"),
            ("1\t2\t1\tdiscard-reload", "unavailable"),
            ("1\t2\t1\tdiscard", "unavailable"),
            ("1\t2\t1\tpath\t61", "unavailable"),
        ] {
            assert!(job_request(
                &mut w,
                &peer,
                &socket,
                &format!("1\t2\tdialog-answer\t{args}")
            )
            .contains(&format!("\terror\t{code}\t")));
            assert_eq!(job_request(&mut w, &peer, &socket, "1\t0\tstate"), before);
        }
        assert_eq!(
            job_request(
                &mut w,
                &peer,
                &socket,
                "1\t3\tdialog-answer\t1\t2\t1\treload"
            ),
            "1\t3\tok\tdialog\t1"
        );
        assert!(w
            .control_dialog_fields()
            .contains("1,conflict,discard,2,1,cancel+discard-reload"));
        for action in ["reload", "save-as\t61"] {
            assert!(job_request(
                &mut w,
                &peer,
                &socket,
                &format!("1\t4\tdialog-answer\t1\t2\t1\t{action}")
            )
            .contains("\terror\tunavailable\t"));
        }
        assert_eq!(
            job_request(
                &mut w,
                &peer,
                &socket,
                "1\t5\tdialog-answer\t1\t2\t1\tdiscard-reload"
            ),
            "1\t5\tpending\t2"
        );
        assert_eq!(w.ui.editor().document(2).unwrap().text(), "adisk");
        assert!(w
            .control_dialog_fields()
            .contains("1,conflict,reloading,2,1,cancel"));
        assert!(job_request(
            &mut w,
            &peer,
            &socket,
            "1\t6\tdialog-answer\t1\t2\t1\tdiscard-reload"
        )
        .contains("\terror\tunavailable\t"));
        finish_file(&mut w);
        assert_eq!(w.ui.editor().active(), Some(3));
        let doc = w.ui.editor().document(2).unwrap();
        assert_eq!(doc.text(), "external");
        assert_eq!(doc.revision(), 2);
        assert!(!doc.dirty());
        assert_eq!(std::fs::read(file).unwrap(), b"external");
        assert!(w.conflict.is_none() && w.reloading.is_none());
        assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
            .contains("job=2,reload,2,1,0,complete,-"));
        w.stop_control();
    }

    #[test]
    fn remote_reload_cancel_rejects_the_read_and_preserves_old_baseline() {
        for (physical, edit) in [(false, false), (false, true), (true, false), (true, true)] {
            let directory = DialogDirectory::new();
            let (mut w, peer, socket, file) = remote_conflict_fixture(&directory, false);
            assert_eq!(
                job_request(
                    &mut w,
                    &peer,
                    &socket,
                    "1\t2\tdialog-answer\t1\t2\t1\treload"
                ),
                "1\t2\tok\tdialog\t1"
            );
            assert_eq!(
                job_request(
                    &mut w,
                    &peer,
                    &socket,
                    "1\t3\tdialog-answer\t1\t2\t1\tdiscard-reload"
                ),
                "1\t3\tpending\t2"
            );
            if physical {
                w.conflict_chord("Escape", false);
            } else {
                assert_eq!(
                    job_request(
                        &mut w,
                        &peer,
                        &socket,
                        "1\t4\tdialog-answer\t1\t2\t1\tcancel"
                    ),
                    "1\t4\tok\t"
                );
            }
            assert!(w.files.as_ref().unwrap().busy());
            assert!(w.reloading.is_none() && w.control_file_job.is_none());
            if edit {
                w.chord("b", false).unwrap();
            }
            finish_file(&mut w);
            assert_eq!(
                w.ui.editor().document(2).unwrap().text(),
                if edit { "abdisk" } else { "adisk" }
            );
            assert!(w.ui.editor().document(2).unwrap().dirty());
            assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
                .contains("job=2,reload,2,1,0,cancelled,-"));
            let revision = if edit { 2 } else { 1 };
            assert_eq!(
                job_request(
                    &mut w,
                    &peer,
                    &socket,
                    &format!("1\t5\tsave\t2\t{revision}")
                ),
                "1\t5\tpending\t3"
            );
            finish_file(&mut w);
            assert_eq!(std::fs::read(&file).unwrap(), b"external");
            assert!(w
                .control_dialog_fields()
                .contains(&format!("2,conflict,question,2,{revision},")));
            assert!(job_request(
                &mut w,
                &peer,
                &socket,
                &format!("1\t6\tdialog-answer\t1\t2\t{revision}\tcancel")
            )
            .contains("\terror\tinvalid-argument\t"));
            assert_eq!(
                job_request(
                    &mut w,
                    &peer,
                    &socket,
                    &format!("1\t7\tdialog-answer\t2\t2\t{revision}\tcancel")
                ),
                "1\t7\tok\t"
            );
            assert!(w.conflict.is_none());
            w.stop_control();
        }
    }

    #[test]
    fn remote_conflict_save_as_preserves_external_file_and_failed_reload_keeps_text() {
        use std::os::unix::ffi::OsStringExt;
        let directory = DialogDirectory::new();
        let (mut w, peer, socket, file) = remote_conflict_fixture(&directory, true);
        let path = directory
            .0
            .join(std::ffi::OsString::from_vec(b"copy-\xff".to_vec()));
        let command = format!(
            "1\t2\tdialog-answer\t1\t2\t1\tsave-as\t{}",
            crate::control::hex(path.as_os_str().as_encoded_bytes())
        );
        assert_eq!(
            job_request(&mut w, &peer, &socket, &command),
            "1\t2\tpending\t2"
        );
        assert!(w.conflict.is_none() && w.prompt.is_none());
        finish_file(&mut w);
        assert_eq!(std::fs::read(file).unwrap(), b"external");
        assert_eq!(std::fs::read(path).unwrap(), b"adisk");
        assert!(!w.ui.editor().document(2).unwrap().dirty());
        assert_eq!(w.ui.editor().active(), Some(3));
        w.stop_control();
        for bytes in [b"\0".as_slice(), b"\xff".as_slice()] {
            let directory = DialogDirectory::new();
            let (mut w, peer, socket, file) = remote_conflict_fixture(&directory, false);
            std::fs::write(&file, bytes).unwrap();
            assert_eq!(
                job_request(
                    &mut w,
                    &peer,
                    &socket,
                    "1\t2\tdialog-answer\t1\t2\t1\treload"
                ),
                "1\t2\tok\tdialog\t1"
            );
            assert_eq!(
                job_request(
                    &mut w,
                    &peer,
                    &socket,
                    "1\t3\tdialog-answer\t1\t2\t1\tdiscard-reload"
                ),
                "1\t3\tpending\t2"
            );
            finish_file(&mut w);
            assert_eq!(w.ui.editor().document(2).unwrap().text(), "adisk");
            assert_eq!(w.ui.editor().document(2).unwrap().revision(), 1);
            assert!(w.ui.editor().document(2).unwrap().dirty());
            assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
                .contains("job=2,reload,2,1,0,error,unavailable"));
            w.stop_control();
        }
    }

    #[test]
    fn remote_clean_reload_is_one_answer_and_conflict_id_exhaustion_is_fail_closed() {
        let directory = DialogDirectory::new();
        let (mut w, peer, socket, _file) = remote_conflict_fixture(&directory, false);
        assert_eq!(
            job_request(
                &mut w,
                &peer,
                &socket,
                "1\t2\tdialog-answer\t1\t2\t1\tcancel"
            ),
            "1\t2\tok\t"
        );
        w.chord("C-z", false).unwrap();
        assert!(!w.ui.editor().document(2).unwrap().dirty());
        assert_eq!(w.ui.editor().document(2).unwrap().revision(), 2);
        assert_eq!(
            job_request(&mut w, &peer, &socket, "1\t3\tsave\t2\t2"),
            "1\t3\tpending\t2"
        );
        finish_file(&mut w);
        assert!(w
            .control_dialog_fields()
            .contains("2,conflict,question,2,2,"));
        assert_eq!(
            job_request(
                &mut w,
                &peer,
                &socket,
                "1\t4\tdialog-answer\t2\t2\t2\treload"
            ),
            "1\t4\tpending\t3"
        );
        finish_file(&mut w);
        let doc = w.ui.editor().document(2).unwrap();
        assert_eq!(doc.text(), "external");
        assert_eq!(doc.revision(), 3);
        assert!(!doc.dirty());
        assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
            .contains("job=3,reload,2,2,0,complete,-"));
        w.stop_control();

        let directory = DialogDirectory::new();
        let (mut w, peer, socket, file) = remote_conflict_fixture(&directory, false);
        assert_eq!(
            job_request(
                &mut w,
                &peer,
                &socket,
                "1\t2\tdialog-answer\t1\t2\t1\tcancel"
            ),
            "1\t2\tok\t"
        );
        w.last_dialog_id = u64::MAX;
        assert_eq!(
            job_request(&mut w, &peer, &socket, "1\t3\tsave\t2\t1"),
            "1\t3\tpending\t2"
        );
        finish_file(&mut w);
        assert!(w.conflict.is_none() && w.reloading.is_none());
        assert_eq!(w.last_dialog_id, u64::MAX);
        assert!(w.notice.as_ref().unwrap().contains("exhausted"));
        assert_eq!(std::fs::read(file).unwrap(), b"external");
        assert_eq!(w.ui.editor().document(2).unwrap().text(), "adisk");
        assert!(w.ui.editor().document(2).unwrap().dirty());
        assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
            .contains("job=2,save,2,1,0,error,unavailable"));
        w.stop_control();
    }

    #[test]
    fn remote_reload_counter_refusals_preserve_the_unconsumed_discard_permit() {
        for guard in ["generation", "jobs"] {
            let directory = DialogDirectory::new();
            let (mut w, peer, socket, _file) = remote_conflict_fixture(&directory, false);
            assert_eq!(
                job_request(
                    &mut w,
                    &peer,
                    &socket,
                    "1\t2\tdialog-answer\t1\t2\t1\treload"
                ),
                "1\t2\tok\tdialog\t1"
            );
            if guard == "generation" {
                w.ui.generation_for_test(u64::MAX);
            } else {
                w.control_jobs.exhaust_for_test();
            }
            let before = job_request(&mut w, &peer, &socket, "1\t0\tstate");
            assert!(job_request(
                &mut w,
                &peer,
                &socket,
                "1\t3\tdialog-answer\t1\t2\t1\tdiscard-reload"
            )
            .contains("\terror\texhausted\t"));
            assert_eq!(job_request(&mut w, &peer, &socket, "1\t0\tstate"), before);
            assert_eq!(
                w.conflict.as_ref().unwrap().target(w.ui.editor()).unwrap(),
                Target {
                    tab: 2,
                    revision: 1
                }
            );
            assert!(w.conflict.as_ref().unwrap().needs_discard());
            assert!(!w.files.as_ref().unwrap().busy());
            w.stop_control();
        }
    }

    #[test]
    fn remote_close_save_targets_unfocused_dialog_and_writes_before_closing() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path("control");
        let file = directory.path("draft");
        std::fs::write(&file, b"disk").unwrap();
        let (mut w, peer) = file_dialog_fixture();
        w.files.as_mut().unwrap().open(file.clone()).unwrap();
        finish_file(&mut w);
        w.chord("a", false).unwrap();
        w.close_tab(2, 1);
        configure(&mut w, 100, 80);
        let device = w.device.unwrap();
        w.event(message(device, 2, &[99, SURFACE])).unwrap();
        assert!(!w.input.focused && !w.close_answer_visible());
        w.close_chord("C-s", false);
        assert!(!w.files.as_ref().unwrap().busy());
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&socket).unwrap(),
            )
            .unwrap(),
        );
        let before = job_request(&mut w, &peer, &socket, "1\t0\tstate");
        for (answer, code) in [
            ("0\t2\t1\tsave", "invalid-argument"),
            ("2\t2\t1\tsave", "invalid-argument"),
            ("1\t1\t1\tsave", "invalid-argument"),
            ("1\t2\t0\tsave", "stale-revision"),
            ("1\t2\t1\tpath\t61", "unavailable"),
        ] {
            assert!(job_request(
                &mut w,
                &peer,
                &socket,
                &format!("1\t1\tdialog-answer\t{answer}")
            )
            .contains(&format!("\terror\t{code}\t")));
            assert_eq!(job_request(&mut w, &peer, &socket, "1\t0\tstate"), before);
        }
        assert_eq!(
            job_request(&mut w, &peer, &socket, "1\t2\tdialog-answer\t1\t2\t1\tsave"),
            "1\t2\tpending\t1"
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"disk");
        assert!(w
            .control_dialog_fields()
            .contains("1,close-tab,saving,2,1,cancel"));
        for answer in ["save", "discard", "path\t61"] {
            assert!(job_request(
                &mut w,
                &peer,
                &socket,
                &format!("1\t3\tdialog-answer\t1\t2\t1\t{answer}")
            )
            .contains("\terror\tunavailable\t"));
        }
        finish_file(&mut w);
        assert_eq!(std::fs::read(file).unwrap(), b"adisk");
        assert!(w.ui.editor().document(2).is_err());
        assert_eq!(w.ui.editor().active(), Some(1));
        assert!(w.closing.is_none());
        assert!(job_request(&mut w, &peer, &socket, "1\t0\tstate")
            .contains("job=1,save,2,1,0,complete,-"));
        w.stop_control();
    }

    #[test]
    fn remote_close_path_preserves_live_identity_and_targets_inactive_window_tabs() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path("control");
        let first = directory.path("first");
        let second = directory
            .0
            .join(std::ffi::OsString::from_vec(b"second-\xff".to_vec()));
        let (mut w, peer) = file_dialog_fixture();
        w.chord("a", false).unwrap();
        w.ui.dispatch(Event::New).unwrap();
        w.chord("b", false).unwrap();
        w.close();
        configure(&mut w, 100, 80);
        let device = w.device.unwrap();
        w.event(message(device, 2, &[99, SURFACE])).unwrap();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&socket).unwrap(),
            )
            .unwrap(),
        );
        for (tab, destination, bytes) in [(2, &second, b"b"), (1, &first, b"a")] {
            assert_eq!(w.ui.editor().active(), Some(2));
            assert_eq!(
                job_request(
                    &mut w,
                    &peer,
                    &socket,
                    &format!("1\t1\tdialog-answer\t1\t{tab}\t1\tsave")
                ),
                "1\t1\tok\tdialog\t1"
            );
            assert!(w
                .control_dialog_fields()
                .contains(&format!("1,close-window,path,{tab},1,cancel+path")));
            w.prompt.as_mut().unwrap().text = "partial native entry".into();
            let before = job_request(&mut w, &peer, &socket, "1\t0\tstate");
            for (args, code) in [
                (format!("1\t{tab}\t1\tsave"), "unavailable"),
                (format!("1\t{tab}\t1\tdiscard"), "unavailable"),
                (format!("1\t{tab}\t0\tpath\t61"), "stale-revision"),
                (format!("2\t{tab}\t1\tpath\t61"), "invalid-argument"),
            ] {
                assert!(job_request(
                    &mut w,
                    &peer,
                    &socket,
                    &format!("1\t2\tdialog-answer\t{args}")
                )
                .contains(&format!("\terror\t{code}\t")));
                assert_eq!(job_request(&mut w, &peer, &socket, "1\t0\tstate"), before);
                assert_eq!(w.prompt.as_ref().unwrap().text, "partial native entry");
            }
            let command = format!(
                "1\t3\tdialog-answer\t1\t{tab}\t1\tpath\t{}",
                crate::control::hex(destination.as_os_str().as_encoded_bytes())
            );
            assert_eq!(
                job_request(&mut w, &peer, &socket, &command),
                format!("1\t3\tpending\t{}", 3 - tab)
            );
            assert!(w.prompt.is_none());
            assert!(!destination.exists());
            finish_file(&mut w);
            assert_eq!(std::fs::read(destination).unwrap(), bytes);
            assert_eq!(w.last_dialog_id, 1);
            if tab == 2 {
                assert!(!w.closed);
                assert!(!w.ui.editor().document(2).unwrap().dirty());
                assert!(w
                    .control_dialog_fields()
                    .contains("1,close-window,question,1,1,cancel+discard+save"));
            }
        }
        assert!(w.closed);
        // Whole-window exit retains the model until the Window owner drops.
        assert_eq!(w.ui.editor().tabs().count(), 2);
        assert!(w.ui.editor().tabs().all(|(_, doc)| !doc.dirty()));
        w.stop_control();
    }

    #[test]
    fn remote_close_save_cancel_keeps_accepted_jobs_and_later_edits() {
        for (handoff, edit) in [(false, false), (false, true), (true, true)] {
            let directory = DialogDirectory::new();
            let file = directory.path("draft");
            std::fs::write(&file, b"disk").unwrap();
            let (mut w, _peer) = file_dialog_fixture();
            w.files.as_mut().unwrap().open(file.clone()).unwrap();
            finish_file(&mut w);
            w.chord("a", false).unwrap();
            w.close_tab(2, 1);
            let answer = |action: &str| {
                crate::control::Request::parse(
                    format!("1\t1\tdialog-answer\t1\t2\t1\t{action}").as_bytes(),
                )
                .unwrap()
            };
            assert_eq!(w.control_response(&answer("save")), "1\t1\tpending\t1");
            if handoff {
                w.tick(w.clock, false).unwrap();
            }
            assert_eq!(w.control_response(&answer("cancel")), "1\t1\tok\t");
            assert!(w.files.as_ref().unwrap().busy());
            assert!(w.closing.is_none() && !w.closing_save);
            if edit {
                w.chord("b", false).unwrap();
            }
            finish_file(&mut w);
            let doc = w.ui.editor().document(2).unwrap();
            assert_eq!(doc.text(), if edit { "abdisk" } else { "adisk" });
            assert_eq!(doc.dirty(), edit);
            assert!(!w.closed);
            let stale = !handoff && edit;
            assert_eq!(
                std::fs::read(file).unwrap(),
                if stale {
                    b"disk".as_slice()
                } else {
                    b"adisk".as_slice()
                }
            );
            let state =
                w.control_response(&crate::control::Request::parse(b"1\t0\tstate").unwrap());
            assert!(state.contains(if stale {
                "job=1,save,2,1,0,error,stale-revision"
            } else {
                "job=1,save,2,1,0,complete,-"
            }));
        }
    }

    #[test]
    fn remote_close_save_refusals_keep_prompt_and_approvals() {
        for guard in ["generation", "jobs"] {
            let (mut w, _peer) = file_dialog_fixture();
            w.chord("a", false).unwrap();
            w.close_tab(1, 1);
            let request = |action: &str| {
                crate::control::Request::parse(
                    format!("1\t1\tdialog-answer\t1\t1\t1\t{action}").as_bytes(),
                )
                .unwrap()
            };
            assert_eq!(w.control_response(&request("save")), "1\t1\tok\tdialog\t1");
            w.prompt.as_mut().unwrap().text = "keep entry".into();
            if guard == "generation" {
                w.ui.generation_for_test(u64::MAX);
            } else {
                w.control_jobs.exhaust_for_test();
            }
            let state = crate::control::Request::parse(b"1\t0\tstate").unwrap();
            let before = w.control_response(&state);
            let notice = w.notice.clone();
            assert!(w
                .control_response(&request("path\t61"))
                .contains("\terror\texhausted\t"));
            assert_eq!(w.control_response(&state), before);
            assert_eq!(w.notice, notice);
            assert_eq!(w.prompt.as_ref().unwrap().text, "keep entry");
            assert!(!w.files.as_ref().unwrap().busy());
            assert!(w.closing.is_some());
        }
    }

    #[test]
    fn remote_close_save_failures_retain_tabs_and_cancel_the_close_plan() {
        for associated in [false, true] {
            let directory = DialogDirectory::new();
            let file = directory.path("draft");
            std::fs::write(&file, b"disk").unwrap();
            let (mut w, _peer) = file_dialog_fixture();
            let tab = if associated {
                w.files.as_mut().unwrap().open(file.clone()).unwrap();
                finish_file(&mut w);
                2
            } else {
                1
            };
            w.chord("a", false).unwrap();
            w.close_tab(tab, 1);
            let request = |action: &str| {
                crate::control::Request::parse(
                    format!("1\t1\tdialog-answer\t1\t{tab}\t1\t{action}").as_bytes(),
                )
                .unwrap()
            };
            if associated {
                std::fs::write(&file, b"external").unwrap();
                assert_eq!(w.control_response(&request("save")), "1\t1\tpending\t1");
            } else {
                assert_eq!(w.control_response(&request("save")), "1\t1\tok\tdialog\t1");
                let path = format!(
                    "path\t{}",
                    crate::control::hex(file.as_os_str().as_encoded_bytes())
                );
                assert_eq!(w.control_response(&request(&path)), "1\t1\tpending\t1");
            }
            finish_file(&mut w);
            assert!(w.closing.is_none() && w.prompt.is_none() && !w.closing_save && !w.closed);
            assert_eq!(w.conflict.is_some(), associated);
            assert!(w.ui.editor().document(tab).unwrap().dirty());
            assert_eq!(
                w.ui.editor().document(tab).unwrap().text(),
                if associated { "adisk" } else { "a" }
            );
            assert_eq!(
                std::fs::read(file).unwrap(),
                if associated {
                    b"external".as_slice()
                } else {
                    b"disk".as_slice()
                }
            );
            let state =
                w.control_response(&crate::control::Request::parse(b"1\t0\tstate").unwrap());
            assert!(state.contains(&format!(
                "job=1,{},{tab},1,0,error,unavailable",
                if associated { "save" } else { "save-as" }
            )));
            assert!(w
                .control_response(&request("save"))
                .contains(if associated {
                    "\terror\tinvalid-argument\t"
                } else {
                    "\terror\tunavailable\t"
                }));
        }
        let (mut w, _peer) = file_dialog_fixture();
        w.chord("a", false).unwrap();
        w.close_tab(1, 1);
        let request = |action: &str| {
            crate::control::Request::parse(
                format!("1\t1\tdialog-answer\t1\t1\t1\t{action}").as_bytes(),
            )
            .unwrap()
        };
        assert_eq!(w.control_response(&request("save")), "1\t1\tok\tdialog\t1");
        w.files = Some(crate::session::Session::disconnected_for_test());
        // Poll observes the failed worker before admission: the accepted job ID
        // reports immediate startup failure and the native close plan is cleared.
        w.tick(w.clock, false).unwrap();
        assert_eq!(w.control_response(&request("path\t61")), "1\t1\tpending\t1");
        assert!(w.closing.is_none() && w.prompt.is_none() && !w.closing_save);
        assert!(w.control_file_job.is_none());
        assert!(w.ui.editor().document(1).unwrap().dirty());
        assert!(w
            .control_response(&crate::control::Request::parse(b"1\t0\tstate").unwrap())
            .contains("job=1,save-as,1,1,0,error,unavailable"));
    }

    #[test]
    fn remote_close_dialog_ids_and_revisions_guard_unfocused_discard() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        w.chord("a", false).unwrap();
        w.ui.dispatch(Event::New).unwrap();
        w.chord("b", false).unwrap();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        let documents = format!("{:?}", w.ui.editor());
        assert_eq!(
            job_request(&mut w, &peer, &path, "1\t1\tclose-tab\t2\t1"),
            "1\t1\tok\tdialog\t1"
        );
        let state = job_request(&mut w, &peer, &path, "1\t0\tstate");
        assert!(state.contains("\tdialog=1,close-tab,question,2,1,cancel+discard+save\t"));
        assert_eq!(format!("{:?}", w.ui.editor()), documents);
        assert_eq!(
            job_request(&mut w, &peer, &path, "1\t2\tdialog-answer\t1\t2\t1\tcancel"),
            "1\t2\tok\t"
        );
        assert_eq!(format!("{:?}", w.ui.editor()), documents);
        assert_eq!(
            job_request(&mut w, &peer, &path, "1\t3\tclose-tab\t2\t1"),
            "1\t3\tok\tdialog\t2"
        );
        configure(&mut w, 100, 80);
        let device = w.device.unwrap();
        w.event(message(device, 2, &[99, SURFACE])).unwrap();
        assert!(!w.input.focused);
        assert!(!w.close_answer_visible());
        w.close_chord("C-d", false); // Physical confirmation is still refused.
        assert_eq!(w.ui.editor().tabs().count(), 2);
        let before = job_request(&mut w, &peer, &path, "1\t0\tstate");
        let notice = w.notice.clone();
        for (args, code) in [
            ("1\t2\t1\tdiscard", "invalid-argument"),
            ("0\t2\t1\tdiscard", "invalid-argument"),
            ("2\t1\t1\tdiscard", "invalid-argument"),
            ("2\t2\t0\tdiscard", "stale-revision"),
        ] {
            assert!(job_request(
                &mut w,
                &peer,
                &path,
                &format!("1\t4\tdialog-answer\t{args}")
            )
            .contains(&format!("\terror\t{code}\t")));
            assert_eq!(job_request(&mut w, &peer, &path, "1\t0\tstate"), before);
            assert_eq!(w.notice, notice);
        }
        assert_eq!(
            job_request(
                &mut w,
                &peer,
                &path,
                "1\t5\tdialog-answer\t2\t2\t1\tdiscard"
            ),
            "1\t5\tok\t"
        );
        assert_eq!(w.ui.editor().active(), Some(1));
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "a");
        assert!(w.ui.editor().document(1).unwrap().dirty());
        assert!(job_request(
            &mut w,
            &peer,
            &path,
            "1\t6\tdialog-answer\t2\t2\t1\tdiscard"
        )
        .contains("\terror\tunavailable\t"));
        w.stop_control();
    }

    #[test]
    fn remote_window_close_answers_preserve_deferred_approvals_and_native_paths() {
        let (mut w, _peer) = file_dialog_fixture();
        configure(&mut w, 800, 600);
        w.chord("a", false).unwrap();
        w.ui.dispatch(Event::New).unwrap();
        w.chord("b", false).unwrap();
        let before = format!("{:?}", w.ui.editor());
        let quit = crate::control::Request::parse(b"1\t0\tquit").unwrap();
        assert_eq!(w.control_response(&quit), "1\t0\tok\tdialog\t1");
        w.close(); // Repeated window-manager close keeps the same coordinator and ID.
        assert_eq!(w.last_dialog_id, 1);
        let request = |args: &str| {
            crate::control::Request::parse(format!("1\t1\tdialog-answer\t{args}").as_bytes())
                .unwrap()
        };
        assert!(w
            .control_dialog_fields()
            .ends_with("dialog=1,close-window,question,2,1,cancel+discard+save"));
        assert_eq!(
            w.control_response(&request("1\t2\t1\tdiscard")),
            "1\t1\tok\t"
        );
        assert_eq!(format!("{:?}", w.ui.editor()), before);
        assert!(w
            .control_dialog_fields()
            .ends_with("dialog=1,close-window,question,1,1,cancel+discard+save"));
        assert!(w
            .control_response(&request("1\t2\t1\tdiscard"))
            .contains("\terror\tinvalid-argument\t"));
        assert_eq!(
            w.control_response(&request("1\t1\t1\tcancel")),
            "1\t1\tok\t"
        );
        assert_eq!(format!("{:?}", w.ui.editor()), before);
        assert!(!w.closed);
        w.close();
        assert_eq!(w.last_dialog_id, 2);
        w.close_chord("C-d", false); // Physical approval advances the remotely visible target.
        assert!(w
            .control_dialog_fields()
            .ends_with("dialog=2,close-window,question,1,1,cancel+discard+save"));
        assert_eq!(
            w.control_response(&request("2\t1\t1\tdiscard")),
            "1\t1\tok\t"
        );
        assert!(w.closed);
    }

    #[test]
    fn remote_close_clean_targets_stale_requests_and_exhaustion_are_bounded() {
        let (mut w, _peer) = file_dialog_fixture();
        w.ui.dispatch(Event::New).unwrap();
        let request = |args: &str| {
            crate::control::Request::parse(format!("1\t1\tclose-tab\t{args}").as_bytes()).unwrap()
        };
        let state = crate::control::Request::parse(b"1\t0\tstate").unwrap();
        let before = w.control_response(&state);
        let notice = w.notice.clone();
        for (args, code) in [
            ("99\t0", "missing-tab"),
            ("1\t0", "invalid-argument"),
            ("2\t1", "stale-revision"),
        ] {
            assert!(w
                .control_response(&request(args))
                .contains(&format!("\terror\t{code}\t")));
            assert_eq!(w.control_response(&state), before);
            assert_eq!(w.notice, notice);
        }
        assert_eq!(w.control_response(&request("2\t0")), "1\t1\tok\tclosed");
        assert_eq!(w.ui.editor().active(), Some(1));
        assert!(w.closing.is_none());
        w.last_dialog_id = u64::MAX;
        let before = w.control_response(&state);
        let notice = w.notice.clone();
        assert!(w
            .control_response(&request("1\t0"))
            .contains("\terror\texhausted\t"));
        assert_eq!(w.control_response(&state), before);
        assert_eq!(w.notice, notice);
        let (mut last, _peer) = file_dialog_fixture();
        assert_eq!(last.control_response(&request("1\t0")), "1\t1\tok\tclosed");
        assert!(last.closed);
    }

    #[test]
    fn remote_dialog_answer_refuses_closed_scratch_and_unrelated_native_modals() {
        let answer =
            crate::control::Request::parse(b"1\t1\tdialog-answer\t1\t1\t1\tdiscard").unwrap();
        let state = crate::control::Request::parse(b"1\t0\tstate").unwrap();
        let (mut w, _peer) = file_dialog_fixture();
        w.chord("a", false).unwrap();
        w.close_tab(1, 1);
        // A closed-adapter fixture retains the live token to isolate this guard.
        w.closed = true;
        let before = w.control_response(&state);
        let notice = w.notice.clone();
        assert!(w
            .control_response(&answer)
            .contains("\terror\tunavailable\t"));
        assert_eq!(w.control_response(&state), before);
        assert_eq!(w.notice, notice);

        let (mut scratch, peer, device) = seat_fixture();
        send_map(&mut scratch, &peer, device, &map_file());
        focus(&mut scratch, device);
        key(&mut scratch, device, 30);
        for quitting in [false, true] {
            if quitting {
                scratch.close();
            }
            let before = scratch.control_response(&state);
            let notice = scratch.notice.clone();
            assert!(scratch
                .control_response(&answer)
                .contains("\terror\tunavailable\t"));
            assert_eq!(scratch.control_response(&state), before);
            assert_eq!(scratch.notice, notice);
        }
        for chord in ["C-o", "C-f", "C-h", "F6", "F10"] {
            let (mut w, _peer) = file_dialog_fixture();
            configure(&mut w, 800, 600);
            w.chord(chord, false).unwrap();
            // Match the live revision so path prompts refuse the answer kind.
            let answer =
                crate::control::Request::parse(b"1\t1\tdialog-answer\t1\t1\t0\tdiscard").unwrap();
            let before = w.control_response(&state);
            let notice = w.notice.clone();
            assert!(w
                .control_response(&answer)
                .contains("\terror\tunavailable\t"));
            assert_eq!(w.control_response(&state), before);
            assert_eq!(w.notice, notice);
        }
    }

    #[test]
    fn physical_discard_retains_close_completion_failure_diagnostic() {
        let (mut w, _peer) = file_dialog_fixture();
        configure(&mut w, 800, 600);
        w.chord("a", false).unwrap();
        w.close_tab(1, 1);
        assert!(w.close_answer_visible());
        w.ui.generation_for_test(u64::MAX);
        w.close_chord("C-d", false);
        assert!(w.closing.is_none());
        assert!(!w.closed);
        assert_eq!(w.notice.as_deref(), Some("Close cancelled: exhausted"));
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "a");
        assert!(w.ui.editor().document(1).unwrap().dirty());
    }

    #[test]
    fn remote_cancel_drops_close_path_and_never_cancels_an_accepted_save() {
        let request = |action: &str| {
            crate::control::Request::parse(
                format!("1\t1\tdialog-answer\t1\t2\t1\t{action}").as_bytes(),
            )
            .unwrap()
        };
        let (mut path_window, _peer) = file_dialog_fixture();
        configure(&mut path_window, 800, 600);
        path_window.ui.dispatch(Event::New).unwrap();
        path_window.chord("a", false).unwrap();
        path_window.close_tab(2, 1);
        path_window.close_chord("C-s", false);
        assert!(path_window
            .control_dialog_fields()
            .ends_with("dialog=1,close-tab,path,2,1,cancel+path"));
        let before = path_window.control_dialog_fields();
        assert!(path_window
            .control_response(&request("discard"))
            .contains("\terror\tunavailable\t"));
        assert_eq!(path_window.control_dialog_fields(), before);
        assert_eq!(
            path_window.control_response(&request("cancel")),
            "1\t1\tok\t"
        );
        assert!(path_window.prompt.is_none() && path_window.closing.is_none());
        assert_eq!(path_window.ui.editor().document(2).unwrap().text(), "a");

        let directory = DialogDirectory::new();
        let path = directory.path("document");
        std::fs::write(&path, b"disk").unwrap();
        let (mut w, _peer) = file_dialog_fixture();
        configure(&mut w, 800, 600);
        w.files.as_mut().unwrap().open(path.clone()).unwrap();
        finish_file(&mut w);
        w.chord("a", false).unwrap();
        w.close_tab(2, 1);
        w.close_chord("C-s", false);
        assert!(w.files.as_ref().unwrap().busy());
        assert!(w
            .control_dialog_fields()
            .contains("dialog=1,close-tab,saving,2,1,cancel"));
        let state = crate::control::Request::parse(b"1\t0\tstate").unwrap();
        let before = w.control_response(&state);
        assert!(w
            .control_response(&request("discard"))
            .contains("\terror\tunavailable\t"));
        assert_eq!(w.control_response(&state), before);
        assert_eq!(w.control_response(&request("cancel")), "1\t1\tok\t");
        assert!(w.closing.is_none() && !w.closing_save);
        assert!(w.files.as_ref().unwrap().busy());
        w.chord("b", false).unwrap();
        finish_file(&mut w);
        assert_eq!(std::fs::read(&path).unwrap(), b"adisk");
        assert_eq!(w.ui.editor().document(2).unwrap().text(), "abdisk");
        assert!(w.ui.editor().document(2).unwrap().dirty());
        assert!(!w.closed);
    }

    #[test]
    fn remote_close_counter_and_edit_undo_races_never_grant_discard() {
        let (mut w, _peer) = file_dialog_fixture();
        w.chord("a", false).unwrap();
        let close = crate::control::Request::parse(b"1\t1\tclose-tab\t1\t1").unwrap();
        let state = crate::control::Request::parse(b"1\t0\tstate").unwrap();
        w.ui.generation_for_test(u64::MAX - 1);
        let before = w.control_response(&state);
        assert!(w.control_response(&close).contains("\terror\texhausted\t"));
        assert_eq!(w.control_response(&state), before);
        assert!(w.closing.is_none());
        let (mut w, _peer) = file_dialog_fixture();
        w.chord("a", false).unwrap();
        assert_eq!(w.control_response(&close), "1\t1\tok\tdialog\t1");
        let discard =
            crate::control::Request::parse(b"1\t2\tdialog-answer\t1\t1\t1\tdiscard").unwrap();
        w.ui.generation_for_test(u64::MAX);
        let before = w.control_response(&state);
        assert!(w
            .control_response(&discard)
            .contains("\terror\texhausted\t"));
        assert_eq!(w.control_response(&state), before);
        assert!(w
            .closing
            .as_ref()
            .unwrap()
            .next(w.ui.editor())
            .unwrap()
            .is_some());
        let (mut w, _peer) = file_dialog_fixture();
        w.chord("a", false).unwrap();
        assert_eq!(w.control_response(&close), "1\t1\tok\tdialog\t1");
        // Inject a validated model transition to exercise the coordinator's late guard.
        w.ui.dispatch(Event::Edit {
            tab: 1,
            revision: 1,
            command: crate::model::Command::Insert("b".into()),
        })
        .unwrap();
        w.ui.dispatch(Event::Edit {
            tab: 1,
            revision: 2,
            command: crate::model::Command::Undo,
        })
        .unwrap();
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "a");
        let before = w.control_response(&state);
        assert!(before.contains("dialog=1,close-tab,invalid,-,-,-"));
        assert!(w
            .control_response(&discard)
            .contains("\terror\tstale-revision\t"));
        assert_eq!(w.control_response(&state), before);
        assert!(!w.closed);
    }

    #[test]
    fn native_spelling_job_socket_refusals_preserve_modal_and_capacity_state() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        w.chord("C-f", false).unwrap();
        let before = job_request(&mut w, &peer, &path, "1\t0\tstate");
        assert!(
            job_request(&mut w, &peer, &path, "1\t1\tcheck-spelling\t99\t99")
                .contains("\terror\tunavailable\t")
        );
        assert_eq!(job_request(&mut w, &peer, &path, "1\t0\tstate"), before);
        w.chord("Escape", false).unwrap();
        w.spelling
            .install(crate::spelling::Dictionary::parse(b"known").unwrap());
        let scan = w.spelling.start(w.ui.editor(), 1, 0).unwrap();
        // Synthetic registry saturation: native one-scan use cannot fill it with pending rows.
        for _ in 0..64 {
            let id = w.control_jobs.begin(1, 0).unwrap();
            w.control_jobs.started(id, Ok(scan)).unwrap();
        }
        let before = job_request(&mut w, &peer, &path, "1\t0\tstate");
        assert!(
            job_request(&mut w, &peer, &path, "1\t2\tcheck-spelling\t1\t0")
                .contains("\terror\tlimit\t")
        );
        assert_eq!(job_request(&mut w, &peer, &path, "1\t0\tstate"), before);
        w.stop_control();
    }

    #[test]
    fn native_remote_new_returns_stable_ids_and_preserves_other_tabs() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        w.chord("a", false).unwrap();
        let old = format!("{:?}", w.ui.editor().document(1).unwrap());
        w.spelling
            .install(crate::spelling::Dictionary::parse(b"known").unwrap());
        w.chord("F7", false).unwrap();
        w.end_turn(w.clock, false).unwrap();
        w.ui.dispatch(Event::Profile(Profile::Emacs)).unwrap();
        w.chord("C-x", false).unwrap();
        assert!(crate::control::state(&w.ui)
            .unwrap()
            .contains("\tprefix=1\t"));
        w.event(message(SHM, 0, &[1])).unwrap();
        configure(&mut w, 800, 600);
        w.draw().unwrap();
        drain(&peer);
        let pixels = w.pixels.clone();
        let generation = w.frames.generation().unwrap();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        let mut client = control_client(&path, b"1\t1\tnew");
        assert_eq!(control_answer(&mut w, &mut client, &peer), "1\t1\tok\t2");
        assert_eq!(w.ui.editor().active(), Some(2));
        assert_eq!(format!("{:?}", w.ui.editor().document(1).unwrap()), old);
        let new = w.ui.editor().document(2).unwrap();
        assert_eq!(new.text(), "");
        assert_eq!(new.revision(), 0);
        assert_eq!(
            new.selection(),
            crate::model::Selection {
                anchor: 0,
                caret: 0
            }
        );
        assert_eq!(new.history_depth(), (0, 0));
        assert!(!new.dirty());
        assert!(crate::control::state(&w.ui)
            .unwrap()
            .contains("\ttab=2,0,0,0,0,0,0,72,0,lf\t"));
        assert!(w
            .control_response(&crate::control::Request::parse(b"1\t0\tstate").unwrap())
            .contains("\tprefix=0\t"));
        assert!(w.frames.generation().unwrap() > generation);
        done(&mut w);
        w.draw().unwrap();
        drain(&peer);
        assert_ne!(w.pixels, pixels);
        assert_eq!(w.files.as_ref().unwrap().labels().count(), 0);
        let marks = w.spelling.snapshot(w.ui.editor(), 1, 1).unwrap();
        assert_eq!(marks.status, crate::spelling::ScanStatus::Complete);
        assert_eq!(marks.marks.len(), 1);
        assert_eq!(marks.marks.first(), Some(&(0..1)));
        // The response names its tab even if physical New runs before state.
        w.ui.dispatch(Event::Profile(Profile::Windows)).unwrap();
        w.chord("C-n", false).unwrap();
        assert_eq!(w.ui.editor().active(), Some(3));
        let mut client = control_client(&path, b"1\t2\tselect-tab\t2\t0");
        assert_eq!(control_answer(&mut w, &mut client, &peer), "1\t2\tok\t");
        assert_eq!(w.ui.editor().active(), Some(2));
        // Exercise the real model cap, not a test-only admission exception.
        for id in 4..=64 {
            let request = crate::control::Request::parse(b"1\t3\tnew").unwrap();
            assert_eq!(w.control_response(&request), format!("1\t3\tok\t{id}"));
        }
        w.ui.dispatch(Event::Profile(Profile::Emacs)).unwrap();
        w.chord("C-x", false).unwrap();
        let state = crate::control::Request::parse(b"1\t0\tstate").unwrap();
        let before = w.control_response(&state);
        let mut client = control_client(&path, b"1\t4\tnew");
        assert!(control_answer(&mut w, &mut client, &peer).contains("\terror\tlimit\t"));
        assert_eq!(w.control_response(&state), before);
        assert_eq!(w.ui.editor().tabs().count(), 64);
        assert_eq!(format!("{:?}", w.ui.editor().document(1).unwrap()), old);
        w.stop_control();
    }

    #[test]
    fn remote_new_preserves_an_inactive_scan_and_real_file_association() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let document = directory.path("document");
        let text = "wrong ".repeat(2048);
        std::fs::write(&document, &text).unwrap();
        let (mut w, peer) = file_dialog_fixture();
        w.files.as_mut().unwrap().open(document.clone()).unwrap();
        finish_file(&mut w);
        w.chord("a", false).unwrap();
        let edited = w.ui.editor().document(2).unwrap().text().to_owned();
        let labels = w
            .files
            .as_ref()
            .unwrap()
            .labels()
            .map(|(id, label)| (id, label.to_owned()))
            .collect::<Vec<_>>();
        assert_eq!(labels.len(), 1);
        w.spelling
            .install(crate::spelling::Dictionary::parse(b"known").unwrap());
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(
            job_request(&mut w, &peer, &path, "1\t1\tcheck-spelling\t2\t1"),
            "1\t1\tpending\t1"
        );
        assert_eq!(
            job_request(&mut w, &peer, &path, "1\t2\tnew"),
            "1\t2\tok\t3"
        );
        assert!(job_request(&mut w, &peer, &path, "1\t0\tstate")
            .contains("job=1,spelling,2,1,1,pending,-"));
        assert_eq!(
            w.files
                .as_ref()
                .unwrap()
                .labels()
                .map(|(id, label)| (id, label.to_owned()))
                .collect::<Vec<_>>(),
            labels
        );
        assert_eq!(std::fs::read_to_string(&document).unwrap(), text);
        for _ in 0..32 {
            if !w.spelling.running() {
                break;
            }
            w.end_turn(w.clock, false).unwrap();
        }
        assert!(!w.spelling.running());
        assert!(job_request(&mut w, &peer, &path, "1\t0\tstate")
            .contains("job=1,spelling,2,1,1,complete,-"));
        assert_eq!(
            job_request(&mut w, &peer, &path, "1\t3\tselect-tab\t2\t1"),
            "1\t3\tok\t"
        );
        w.chord("C-s", false).unwrap();
        assert!(w.prompt.is_none()); // Existing association, not Save As.
        finish_file(&mut w);
        assert_eq!(std::fs::read_to_string(&document).unwrap(), edited);
        assert!(!w.ui.editor().document(2).unwrap().dirty());
        assert!(!w.ui.editor().document(3).unwrap().dirty());
        assert_eq!(w.ui.editor().document(3).unwrap().text(), "");
        w.stop_control();
    }

    #[test]
    fn native_spelling_jobs_report_admission_completion_cancellation_and_history() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        let text = "known bad ".repeat(2048);
        w.ui.dispatch(Event::Load(text.as_bytes())).unwrap();
        w.ui.dispatch(Event::Profile(Profile::Emacs)).unwrap();
        w.chord("C-x", false).unwrap();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        assert_eq!(
            job_request(&mut w, &peer, &path, "1\t1\tcheck-spelling\t2\t0"),
            "1\t1\tpending\t1"
        );
        let state = job_request(&mut w, &peer, &path, "1\t0\tstate");
        assert!(state.contains("job=1,spelling,2,0,0,error,unavailable"));
        assert!(state.contains("\tprefix=0\t"));
        assert_eq!(w.ui.editor().document(2).unwrap().text(), text);
        assert!(w.notice.as_ref().unwrap().contains("no dictionary"));
        w.spelling
            .install(crate::spelling::Dictionary::parse(b"known").unwrap());
        assert_eq!(
            job_request(&mut w, &peer, &path, "1\t2\tcheck-spelling\t2\t0"),
            "1\t2\tpending\t2"
        );
        let state = job_request(&mut w, &peer, &path, "1\t0\tstate");
        assert!(state.contains("job=2,spelling,2,0,1,pending,-"));
        assert_eq!(
            job_request(
                &mut w,
                &peer,
                &path,
                "1\t3\tspelling-results\t2\t0\t1\t0\t2"
            ),
            "1\t3\tok\t2\t0\t1\tchecking\t0\t0\t-\t-\t-\t-\t-"
        );
        w.event(message(WM, 0, &[77])).unwrap();
        assert!(drain(&peer).0.contains(&message(WM, 3, &[77])));
        w.chord("F7", false).unwrap(); // An ordinary recheck cancels the old remote job.
        assert!(job_request(&mut w, &peer, &path, "1\t0\tstate")
            .contains("job=2,spelling,2,0,1,cancelled,-"));
        assert_eq!(
            job_request(&mut w, &peer, &path, "1\t4\tcheck-spelling\t2\t0"),
            "1\t4\tpending\t3"
        );
        w.chord("Escape", false).unwrap();
        w.end_turn(w.clock, false).unwrap();
        assert!(job_request(&mut w, &peer, &path, "1\t0\tstate")
            .contains("job=3,spelling,2,0,3,cancelled,-"));
        assert_eq!(
            job_request(&mut w, &peer, &path, "1\t5\tcheck-spelling\t2\t0"),
            "1\t5\tpending\t4"
        );
        assert_eq!(
            job_request(&mut w, &peer, &path, "1\t6\tinsert\t2\t0\t0\t0\t62"),
            "1\t6\tok\t"
        );
        assert!(job_request(&mut w, &peer, &path, "1\t0\tstate")
            .contains("job=4,spelling,2,0,4,error,stale-revision"));
        assert_eq!(
            job_request(&mut w, &peer, &path, "1\t7\tcheck-spelling\t2\t1"),
            "1\t7\tpending\t5"
        );
        for _ in 0..32 {
            if !w.spelling.running() {
                break;
            }
            w.end_turn(w.clock, false).unwrap();
        }
        assert!(!w.spelling.running());
        assert!(job_request(&mut w, &peer, &path, "1\t0\tstate")
            .contains("job=5,spelling,2,1,5,complete,-"));
        assert!(job_request(
            &mut w,
            &peer,
            &path,
            "1\t8\tspelling-results\t2\t1\t5\t0\t2"
        )
        .contains("\t5\tcomplete\t2\t2049\t4096\t2049\t0\t0\t"));
        assert_eq!(
            job_request(&mut w, &peer, &path, "1\t9\tundo\t2\t1"),
            "1\t9\tok\t"
        );
        assert!(job_request(&mut w, &peer, &path, "1\t0\tstate")
            .contains("job=5,spelling,2,1,5,complete,-"));
        assert_eq!(
            job_request(&mut w, &peer, &path, "1\t10\tcheck-spelling\t2\t2"),
            "1\t10\tpending\t6"
        );
        w.spelling
            .install(crate::spelling::Dictionary::parse(b"bad").unwrap());
        w.end_turn(w.clock, false).unwrap();
        assert!(job_request(&mut w, &peer, &path, "1\t0\tstate")
            .contains("job=6,spelling,2,2,6,cancelled,-"));
        let before = job_request(&mut w, &peer, &path, "1\t0\tstate");
        for (command, code) in [
            ("check-spelling\t1\t0", "invalid-argument"),
            ("check-spelling\t2\t0", "stale-revision"),
            ("check-spelling\t99\t0", "missing-tab"),
        ] {
            assert!(
                job_request(&mut w, &peer, &path, &format!("1\t11\t{command}"))
                    .contains(&format!("\terror\t{code}\t"))
            );
            assert_eq!(job_request(&mut w, &peer, &path, "1\t0\tstate"), before);
        }
        w.chord("C-x", false).unwrap();
        w.control_jobs.exhaust_for_test();
        let before = job_request(&mut w, &peer, &path, "1\t0\tstate");
        assert!(
            job_request(&mut w, &peer, &path, "1\t12\tcheck-spelling\t2\t2")
                .contains("\terror\texhausted\t")
        );
        assert_eq!(job_request(&mut w, &peer, &path, "1\t0\tstate"), before);
        w.stop_control();
    }

    #[test]
    fn native_control_socket_edits_share_keyboard_history_and_reject_stale_selection() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        let mut client = control_client(&path, b"1\t30\tinsert\t1\t0\t0\t0\t61cebb");
        assert_eq!(control_answer(&mut w, &mut client, &peer), "1\t30\tok\t");
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "aλ");
        w.chord("C-z", false).unwrap();
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "");
        let mut client = control_client(&path, b"1\t31\tredo\t1\t2");
        assert_eq!(control_answer(&mut w, &mut client, &peer), "1\t31\tok\t");
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "aλ");
        let mut client = control_client(&path, b"1\t32\tinsert\t1\t1\t3\t3\t62");
        assert!(control_answer(&mut w, &mut client, &peer).contains("\terror\tstale-revision\t"));
        w.chord("Home", false).unwrap();
        let mut client = control_client(&path, b"1\t33\tinsert\t1\t3\t3\t3\t62");
        assert!(control_answer(&mut w, &mut client, &peer).contains("\terror\tinvalid-argument\t"));
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "aλ");
        w.chord("C-f", false).unwrap();
        let mut client = control_client(&path, b"1\t34\tdelete\t1\t3\t0\t0");
        assert!(control_answer(&mut w, &mut client, &peer).contains("\terror\tunavailable\t"));
        assert!(w.search.is_some());
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "aλ");
    }

    #[test]
    fn native_control_edits_match_both_key_profiles_and_pixels() {
        use std::os::unix::fs::PermissionsExt;
        for profile in [Profile::Windows, Profile::Emacs] {
            let directory = DialogDirectory::new();
            std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
            let path = directory.path("control");
            let (mut remote, peer) = file_dialog_fixture();
            let (mut local, local_peer) = file_dialog_fixture();
            for w in [&mut remote, &mut local] {
                w.ui.dispatch(Event::Profile(profile)).unwrap();
                configure(w, 800, 600);
                w.event(message(SHM, 0, &[1])).unwrap();
                w.draw().unwrap();
            }
            drain(&peer);
            drain(&local_peer);
            let before = remote.pixels.clone();
            remote.control = Some(
                crate::control_worker::Worker::start(
                    crate::control_socket::Socket::bind(&path).unwrap(),
                )
                .unwrap(),
            );
            let mut client = control_client(&path, b"1\t1\tinsert\t1\t0\t0\t0\t61");
            assert_eq!(
                control_answer(&mut remote, &mut client, &peer),
                "1\t1\tok\t"
            );
            let device = local.device.unwrap();
            key(&mut local, device, 30); // Real translated 'a', not a direct edit.
            assert_eq!(
                crate::control::state(&remote.ui),
                crate::control::state(&local.ui)
            );
            for (w, p) in [(&mut remote, &peer), (&mut local, &local_peer)] {
                done(w);
                w.draw().unwrap();
                drain(p);
            }
            assert_ne!(remote.pixels, before);
            assert_eq!(remote.pixels, local.pixels);
            // A remote Undo must restore the same saved state as a logical key.
            let mut client = control_client(&path, b"1\t2\tundo\t1\t1");
            assert_eq!(
                control_answer(&mut remote, &mut client, &peer),
                "1\t2\tok\t"
            );
            local
                .chord(
                    if profile == Profile::Windows {
                        "C-z"
                    } else {
                        "C-/"
                    },
                    false,
                )
                .unwrap();
            assert_eq!(
                crate::control::state(&remote.ui),
                crate::control::state(&local.ui)
            );
            assert!(!remote.ui.editor().document(1).unwrap().dirty());
        }
    }

    #[test]
    fn native_control_socket_selects_tabs_ranges_deletes_and_fills_atomically() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        w.ui.dispatch(Event::Load("one   two\nthree λ".as_bytes()))
            .unwrap();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        for (command, text) in [
            ("fill-paragraph\t2\t0\t0\t0", "one two three λ"),
            ("undo\t2\t1", "one   two\nthree λ"),
            ("select-range\t2\t2\t18\t16", "one   two\nthree λ"),
            ("delete\t2\t2\t18\t16", "one   two\nthree "),
            ("select-tab\t1\t0", "one   two\nthree "),
        ] {
            let mut client = control_client(&path, format!("1\t1\t{command}").as_bytes());
            assert_eq!(
                control_answer(&mut w, &mut client, &peer),
                "1\t1\tok\t",
                "{command}"
            );
            assert_eq!(w.ui.editor().document(2).unwrap().text(), text);
        }
        assert_eq!(w.ui.editor().active(), Some(1));
        let mut client = control_client(&path, b"1\t2\tundo\t2\t3");
        assert!(control_answer(&mut w, &mut client, &peer).contains("\terror\tinvalid-argument\t"));
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "");
        assert_eq!(
            w.ui.editor().document(2).unwrap().text(),
            "one   two\nthree "
        );
    }

    #[test]
    fn remote_selection_cancels_paste_and_repeat_only_after_success() {
        let (mut w, peer, keyboard, device) = clipboard_fixture();
        selection_offer(&mut w, device, 0xff00_0010, &[crate::data::UTF8]);
        w.clipboard_request("paste", 1, 0).unwrap();
        let (_, _writer) = drain(&peer);
        let before = crate::control::state(&w.ui);
        let refused = crate::control::Request::parse(b"1\t1\tselect-range\t1\t9\t0\t0").unwrap();
        assert!(w
            .control_response(&refused)
            .contains("\terror\tstale-revision\t"));
        assert!(w.clipboard.incoming.is_some());
        assert_eq!(crate::control::state(&w.ui), before);
        // Successful semantic selection cancels even a value-identical paste target.
        let accepted = crate::control::Request::parse(b"1\t2\tselect-range\t1\t0\t2\t0").unwrap();
        assert_eq!(w.control_response(&accepted), "1\t2\tok\t");
        assert!(w.clipboard.incoming.is_none());
        assert_eq!(
            w.notice.as_deref(),
            Some("Paste cancelled: remote control command accepted.")
        );
        w.event(message(keyboard, 5, &[20, 10])).unwrap();
        w.event(message(keyboard, 3, &[1, 0, 30, 1])).unwrap();
        assert!(w.input.repeat(w.clock + 20).unwrap().is_some());
        let revision = w.ui.editor().document(1).unwrap().revision();
        let request =
            crate::control::Request::parse(format!("1\t3\tselect-tab\t1\t{revision}").as_bytes())
                .unwrap();
        assert_eq!(w.control_response(&request), "1\t3\tok\t");
        assert!(w.input.repeat(w.clock + 100).unwrap().is_none());
    }

    #[test]
    fn remote_edits_refuse_native_prompts_and_menu_without_dismissing_or_confirming() {
        for chord in ["C-o", "C-w", "C-f", "F6", "C-h", "F10"] {
            let (mut w, _peer) = file_dialog_fixture();
            w.chord("a", false).unwrap();
            w.chord(chord, false).unwrap();
            assert!(w.pointer_modal() || w.menu.is_some(), "{chord}");
            let state = crate::control::Request::parse(b"1\t0\tstate").unwrap();
            let before = w.control_response(&state);
            let notice = w.notice.clone();
            for command in [
                "save\t1\t1",
                "save-as\t1\t1\t61",
                "open\t61",
                "new",
                "close-tab\t1\t1",
                "quit",
                "select-tab\t1\t1",
                "select-range\t1\t1\t0\t0",
                "insert\t1\t1\t1\t1\t62",
                "delete\t1\t1\t1\t1",
                "undo\t1\t1",
                "redo\t1\t1",
                "fill-paragraph\t1\t1\t1\t1",
                "set-auto-fill\t1\t1\t1",
                "set-fill-column\t1\t1\t40",
                "go-to-line\t1\t1\t1",
                "set-key-profile\t1\t1\temacs",
                "find\t1\t1\t1\t1\t61\t0\t1",
                "replace\t1\t1\t61\t62",
                "check-spelling\t1\t1",
            ] {
                let request =
                    crate::control::Request::parse(format!("1\t1\t{command}").as_bytes()).unwrap();
                assert!(
                    w.control_response(&request)
                        .contains("\terror\tunavailable\t"),
                    "{chord}: {command}"
                );
                assert_eq!(w.control_response(&state), before);
                assert_eq!(w.notice, notice);
            }
            assert_eq!(w.ui.editor().document(1).unwrap().text(), "a");
        }
    }

    #[test]
    fn remote_edit_refuses_quitting_through_the_shared_modal_guard() {
        let (mut w, peer, device) = seat_fixture();
        send_map(&mut w, &peer, device, &map_file());
        focus(&mut w, device);
        key(&mut w, device, 30);
        w.close(); // Scratch's real dirty-close path sets quitting, not closing.
        assert!(w.quitting && !w.closed && w.closing.is_none());
        let tab = w.ui.editor().active().unwrap();
        let revision = w.ui.editor().document(tab).unwrap().revision();
        let state = crate::control::Request::parse(b"1\t0\tstate").unwrap();
        let before = w.control_response(&state);
        for command in [
            format!("undo\t{tab}\t{revision}"),
            format!("check-spelling\t{tab}\t{revision}"),
            "new".into(),
        ] {
            let request =
                crate::control::Request::parse(format!("1\t1\t{command}").as_bytes()).unwrap();
            assert!(w
                .control_response(&request)
                .contains("\terror\tunavailable\t"));
            assert_eq!(w.control_response(&state), before);
        }
        assert!(w.quitting && !w.closed);
    }

    #[test]
    fn remote_edits_invalidate_spelling_without_rechecking_and_preserve_disk_bytes() {
        let directory = DialogDirectory::new();
        let path = directory.path("document");
        std::fs::write(&path, b"wrong").unwrap();
        let (mut w, _peer) = file_dialog_fixture();
        w.files.as_mut().unwrap().open(path.clone()).unwrap();
        finish_file(&mut w);
        let tab = w.ui.editor().active().unwrap();
        w.spelling
            .install(crate::spelling::Dictionary::parse(b"known").unwrap());
        w.chord("F7", false).unwrap();
        w.end_turn(w.clock, false).unwrap();
        assert_eq!(w.spelling.view(w.ui.editor()).1.len(), 1);
        assert_eq!(w.spelling.view(w.ui.editor()).1.first(), Some(&(0..5)));
        let request =
            crate::control::Request::parse(format!("1\t1\tinsert\t{tab}\t0\t0\t0\t61").as_bytes())
                .unwrap();
        assert_eq!(w.control_response(&request), "1\t1\tok\t");
        assert!(w.spelling.view(w.ui.editor()).1.is_empty());
        assert!(!w.spelling.running());
        assert_eq!(std::fs::read(path).unwrap(), b"wrong");
        assert!(w.ui.editor().document(tab).unwrap().dirty());
    }

    #[test]
    fn native_control_modes_drive_key_bindings_without_changing_text_or_spelling() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        let text = "one two three four five\nbad";
        w.ui.dispatch(Event::Load(text.as_bytes())).unwrap();
        let tab = w.ui.editor().active().unwrap();
        w.spelling
            .install(crate::spelling::Dictionary::parse(b"known").unwrap());
        w.chord("F7", false).unwrap();
        w.end_turn(w.clock, false).unwrap();
        let marks = w.spelling.view(w.ui.editor()).1.to_vec();
        assert!(!marks.is_empty());
        w.event(message(SHM, 0, &[1])).unwrap();
        configure(&mut w, 800, 600);
        w.draw().unwrap();
        drain(&peer);
        let pixels = w.pixels.clone();
        let generation = w.frames.generation().unwrap();
        for (id, command, value) in [
            (1, "set-auto-fill", "1"),
            (2, "set-fill-column", "20"),
            (3, "go-to-line", "2"),
            (4, "set-key-profile", "emacs"),
        ] {
            let mut client = control_client(
                &path,
                format!("1\t{id}\t{command}\t{tab}\t0\t{value}").as_bytes(),
            );
            assert_eq!(
                control_answer(&mut w, &mut client, &peer),
                format!("1\t{id}\tok\t")
            );
        }
        let doc = w.ui.editor().document(tab).unwrap();
        assert!(doc.auto_fill());
        assert_eq!(doc.fill_column(), 20);
        assert_eq!(
            doc.selection(),
            crate::model::Selection {
                anchor: 24,
                caret: 24
            }
        );
        assert_eq!(doc.text(), text);
        assert_eq!(doc.revision(), 0);
        assert_eq!(doc.history_depth(), (0, 0));
        assert!(!doc.dirty());
        assert!(w.frames.generation().unwrap() > generation);
        done(&mut w);
        w.draw().unwrap();
        drain(&peer);
        assert_ne!(w.pixels, pixels); // No physical chord has intervened.
        w.chord("Right", false).unwrap();
        w.chord("Right", false).unwrap();
        assert_eq!(w.ui.editor().document(tab).unwrap().selection().caret, 26);
        w.chord("C-a", false).unwrap(); // Emacs: logical line beginning.
        assert_eq!(
            w.ui.editor().document(tab).unwrap().selection(),
            crate::model::Selection {
                anchor: 24,
                caret: 24
            }
        );
        w.chord("C-x", false).unwrap();
        assert!(crate::control::state(&w.ui)
            .unwrap()
            .contains("\tprefix=1\t"));
        let mut client = control_client(
            &path,
            format!("1\t5\tset-key-profile\t{tab}\t0\twindows").as_bytes(),
        );
        assert_eq!(control_answer(&mut w, &mut client, &peer), "1\t5\tok\t");
        assert!(crate::control::state(&w.ui)
            .unwrap()
            .contains("\tprefix=0\t"));
        w.chord("C-a", false).unwrap(); // Windows: select all.
        assert_eq!(
            w.ui.editor().document(tab).unwrap().selection(),
            crate::model::Selection {
                anchor: 0,
                caret: text.len()
            }
        );
        assert_eq!(w.spelling.view(w.ui.editor()).1, marks);
        assert!(!w.spelling.running());
        let mut client = control_client(
            &path,
            format!("1\t6\tselect-range\t{tab}\t0\t23\t23").as_bytes(),
        );
        assert_eq!(control_answer(&mut w, &mut client, &peer), "1\t6\tok\t");
        w.chord("Space", false).unwrap(); // The remote Auto Fill/column controls affect typing.
        let doc = w.ui.editor().document(tab).unwrap();
        assert_eq!(doc.text(), "one two three four\nfive \nbad");
        assert_eq!(doc.history_depth(), (1, 0));
        w.stop_control();
    }

    #[test]
    fn native_idempotent_mode_admission_cancels_paste_and_requests_redraw() {
        let (mut w, peer, _keyboard, device) = clipboard_fixture();
        assert!(!w.ui.editor().document(1).unwrap().auto_fill());
        selection_offer(&mut w, device, 0xff00_0010, &[crate::data::UTF8]);
        w.clipboard_request("paste", 1, 0).unwrap();
        let (_, _writer) = drain(&peer);
        assert!(w.clipboard.incoming.is_some());
        let generation = w.frames.generation().unwrap();
        let request = crate::control::Request::parse(b"1\t1\tset-auto-fill\t1\t0\t0").unwrap();
        assert_eq!(w.control_response(&request), "1\t1\tok\t");
        assert!(w.clipboard.incoming.is_none());
        assert_eq!(
            w.notice.as_deref(),
            Some("Paste cancelled: remote control command accepted.")
        );
        assert!(w.frames.generation().unwrap() > generation);
        assert!(!w.ui.editor().document(1).unwrap().auto_fill());
        assert_eq!(w.ui.editor().document(1).unwrap().revision(), 0);
    }

    #[test]
    fn native_remote_search_replacement_and_undo_preserve_the_shared_paths() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        w.ui.dispatch(Event::Load("λ bad λ bad".as_bytes()))
            .unwrap();
        w.spelling
            .install(crate::spelling::Dictionary::parse(b"known").unwrap());
        w.chord("F7", false).unwrap();
        w.end_turn(w.clock, false).unwrap();
        let marks = w.spelling.view(w.ui.editor()).1.to_vec();
        assert!(!marks.is_empty());
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        w.find(2, 0, "λ", true);
        assert_eq!(
            w.notice.as_deref(),
            Some("Reached start; repeat this search to wrap.")
        );
        let mut noop = control_client(&path, b"1\t90\treplace\t2\t0\t6d697373696e67\t78");
        assert_eq!(control_answer(&mut w, &mut noop, &peer), "1\t90\tok\t");
        assert_eq!(w.searches.query(), "λ");
        w.find(2, 0, "λ", true);
        assert_eq!(w.notice.as_deref(), Some("Search wrapped; match found."));
        let mut reset = control_client(&path, b"1\t91\tselect-range\t2\t0\t0\t0");
        assert_eq!(control_answer(&mut w, &mut reset, &peer), "1\t91\tok\t");
        w.event(message(SHM, 0, &[1])).unwrap();
        configure(&mut w, 800, 600);
        w.draw().unwrap();
        drain(&peer);
        let pixels = w.pixels.clone();
        for (id, command) in [
            (1, "find\t2\t0\t0\t0\t626164\t0\t0"),
            (2, "find\t2\t0\t3\t6\t626164\t0\t0"),
        ] {
            let mut client = control_client(&path, format!("1\t{id}\t{command}").as_bytes());
            assert_eq!(
                control_answer(&mut w, &mut client, &peer),
                format!("1\t{id}\tok\t")
            );
        }
        assert_eq!(
            w.ui.editor().document(2).unwrap().selection().range(),
            10..13
        );
        assert_eq!(w.spelling.view(w.ui.editor()).1, marks);
        let state = crate::control::Request::parse(b"1\t0\tstate").unwrap();
        let before = w.control_response(&state);
        let mut missing = control_client(&path, b"1\t3\tfind\t2\t0\t10\t13\t626164\t0\t0");
        assert_eq!(
            control_answer(&mut w, &mut missing, &peer),
            "1\t3\terror\tno-match\t6e6f2d6d61746368"
        );
        assert_eq!(w.control_response(&state), before);
        let mut wrapped = control_client(&path, b"1\t4\tfind\t2\t0\t10\t13\t626164\t0\t1");
        assert_eq!(control_answer(&mut w, &mut wrapped, &peer), "1\t4\tok\t");
        assert_eq!(w.ui.editor().document(2).unwrap().selection().range(), 3..6);
        assert_eq!(w.searches.query(), "λ"); // Semantic search neither seeds nor clears F3 history.
        done(&mut w);
        w.draw().unwrap();
        drain(&peer);
        assert_ne!(w.pixels, pixels); // Selection-only redraw retains marks.
        let mut replace = control_client(&path, b"1\t5\treplace\t2\t0\t626164\t676f6f64");
        assert_eq!(control_answer(&mut w, &mut replace, &peer), "1\t5\tok\t");
        let doc = w.ui.editor().document(2).unwrap();
        assert_eq!(doc.text(), "λ good λ good");
        assert_eq!(
            doc.selection(),
            crate::model::Selection {
                anchor: 15,
                caret: 15
            }
        );
        assert_eq!(doc.revision(), 1);
        assert_eq!(doc.history_depth(), (1, 0));
        assert!(w.spelling.view(w.ui.editor()).1.is_empty());
        assert!(!w.spelling.running());
        let mut undo = control_client(&path, b"1\t6\tundo\t2\t1");
        assert_eq!(control_answer(&mut w, &mut undo, &peer), "1\t6\tok\t");
        let doc = w.ui.editor().document(2).unwrap();
        assert_eq!(doc.text(), "λ bad λ bad");
        assert_eq!(doc.selection().range(), 3..6);
        assert!(!doc.dirty());
        assert_eq!(doc.revision(), 2);
        let mut stale = control_client(&path, b"1\t7\treplace\t2\t0\t626164\t78");
        assert!(control_answer(&mut w, &mut stale, &peer).contains("\terror\tstale-revision\t"));
        w.find(2, 2, "bad", true);
        assert_eq!(
            w.notice.as_deref(),
            Some("Reached start; repeat this search to wrap.")
        );
        // Two admissions in one outer turn: no tick may repair invalidation.
        let moved = crate::control::Request::parse(b"1\t92\tfind\t2\t2\t3\t6\tcebb\t0\t0").unwrap();
        assert_eq!(w.control_response(&moved), "1\t92\tok\t");
        let reset = crate::control::Request::parse(b"1\t93\tselect-range\t2\t2\t3\t6").unwrap();
        assert_eq!(w.control_response(&reset), "1\t93\tok\t");
        w.find(2, 2, "bad", true);
        assert_eq!(
            w.notice.as_deref(),
            Some("Reached start; repeat this search to wrap.")
        );
        w.stop_control();
    }

    #[test]
    fn native_control_socket_spelling_pages_match_marks_and_survive_modals() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        w.ui.dispatch(Event::Load("naïve bad wrong".as_bytes()))
            .unwrap();
        let tab = w.ui.editor().active().unwrap();
        let mut client = control_client(
            &path,
            format!("1\t0\tspelling-results\t{tab}\t0\t0\t0\t1").as_bytes(),
        );
        assert_eq!(
            control_answer(&mut w, &mut client, &peer),
            format!("1\t0\tok\t{tab}\t0\t0\tno-dictionary\t0\t0\t-\t-\t-\t-\t-")
        );
        w.spelling
            .install(crate::spelling::Dictionary::parse(b"known").unwrap());
        w.chord("F7", false).unwrap();
        let pending = crate::control::Request::parse(
            format!("1\t1\tspelling-results\t{tab}\t0\t0\t0\t1").as_bytes(),
        )
        .unwrap();
        assert!(w
            .control_response(&pending)
            .contains("\t1\tchecking\t0\t0\t-\t-\t-\t-\t-"));
        while w.spelling.running() {
            w.end_turn(w.clock, false).unwrap();
        }
        assert_eq!(w.spelling.view(w.ui.editor()).1, &[7..10, 11..16]);
        w.chord("C-f", false).unwrap();
        let state = crate::control::Request::parse(b"1\t0\tstate").unwrap();
        let before = w.control_response(&state);
        let notice = w.notice.clone();
        let mut client = control_client(
            &path,
            format!("1\t1\tspelling-results\t{tab}\t0\t0\t0\t1").as_bytes(),
        );
        assert_eq!(
            control_answer(&mut w, &mut client, &peer),
            format!("1\t1\tok\t{tab}\t0\t1\tcomplete\t1\t2\t2\t2\t1\t0\t7,10")
        );
        let mut client = control_client(
            &path,
            format!("1\t2\tspelling-results\t{tab}\t0\t1\t1\t1").as_bytes(),
        );
        assert_eq!(
            control_answer(&mut w, &mut client, &peer),
            format!("1\t2\tok\t{tab}\t0\t1\tcomplete\t2\t2\t2\t2\t1\t0\t11,16")
        );
        assert_eq!(w.control_response(&state), before);
        assert_eq!(w.notice, notice);
        assert!(w.search.is_some());
        w.chord("Escape", false).unwrap();
        let mut client = control_client(
            &path,
            format!("1\t3\tinsert\t{tab}\t0\t0\t0\t78").as_bytes(),
        );
        assert_eq!(control_answer(&mut w, &mut client, &peer), "1\t3\tok\t");
        let mut client = control_client(
            &path,
            format!("1\t4\tspelling-results\t{tab}\t0\t1\t1\t1").as_bytes(),
        );
        assert!(control_answer(&mut w, &mut client, &peer).contains("\terror\tstale-revision\t"));
        let mut client = control_client(
            &path,
            format!("1\t5\tspelling-results\t{tab}\t1\t0\t0\t1").as_bytes(),
        );
        assert_eq!(
            control_answer(&mut w, &mut client, &peer),
            format!("1\t5\tok\t{tab}\t1\t0\tnot-checked\t0\t0\t-\t-\t-\t-\t-")
        );
        assert!(!w.spelling.running());
        assert!(w.spelling.view(w.ui.editor()).1.is_empty());
        w.stop_control();
        assert!(!path.exists());
    }

    #[test]
    fn native_frame_waits_acknowledge_rendered_revisions_not_later_edits_or_releases() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        w.event(message(SHM, 0, &[1])).unwrap();
        configure(&mut w, 800, 600);
        w.draw().unwrap();
        drain(&peer);
        let old = w.frames.capture(&w.ui).unwrap();
        let pixels = w.pixels.clone();
        let state = crate::control::Request::parse(b"1\t90\tstate").unwrap();
        let before = w.control_response(&state);
        for (id, target) in [(80, 0), (81, u64::MAX)] {
            let mut invalid =
                control_client(&path, format!("1\t{id}\twait-frame\t{target}").as_bytes());
            assert!(control_answer(&mut w, &mut invalid, &peer)
                .starts_with(&format!("1\t{id}\terror\tinvalid-argument\t")));
            assert!(w.frame_waiters.is_empty());
            assert_eq!(w.control_response(&state), before);
        }
        let mut query = control_client(&path, b"1\t90\tstate");
        let response = control_answer(&mut w, &mut query, &peer);
        assert!(response.contains(&format!(
            "\tframe-submitted={}\tframe-completed=-",
            old.fields()
        )));
        let mut first = control_client(
            &path,
            format!("1\t1\twait-frame\t{}", old.generation()).as_bytes(),
        );
        hold_frame_waits(&mut w, &peer, 1);
        w.chord("a", false).unwrap();
        let target = w.frames.generation().unwrap();
        assert!(target > old.generation());
        let mut second = control_client(&path, format!("1\t2\twait-frame\t{target}").as_bytes());
        hold_frame_waits(&mut w, &peer, 2);
        let mut byte = [0];
        assert_eq!(
            first.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let buffer = w.buffers.first().unwrap().id;
        w.event(message(buffer, 0, &[])).unwrap(); // Release alone cannot satisfy either fence.
        w.control_tick();
        assert_eq!(w.frame_waiters.len(), 2);
        assert_eq!(w.pixels, pixels);
        done(&mut w);
        w.control_tick();
        assert_eq!(w.frame_waiters.len(), 1);
        assert_eq!(
            control_answer(&mut w, &mut first, &peer),
            format!("1\t1\tok\t{}", old.fields())
        );
        let mut query = control_client(&path, b"1\t91\tstate");
        assert!(control_answer(&mut w, &mut query, &peer)
            .ends_with(&format!("\tframe-completed={}", old.fields())));
        assert_eq!(
            second.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        w.draw().unwrap();
        drain(&peer);
        let new = w.frames.capture(&w.ui).unwrap();
        assert!(new.generation() >= target);
        assert_ne!(w.pixels, pixels);
        assert_eq!(w.frames.wait(target), Ok(None));
        done(&mut w);
        assert_eq!(
            control_answer(&mut w, &mut second, &peer),
            format!("1\t2\tok\t{}", new.fields())
        );
        let mut query = control_client(&path, b"1\t92\tstate");
        assert!(control_answer(&mut w, &mut query, &peer).contains(&format!(
            "\tframe-submitted={}\tframe-completed={}",
            new.fields(),
            new.fields()
        )));
        assert!(w.buffers.iter().any(|buffer| buffer.busy)); // Callback is not release.
        w.stop_control();
        assert!(!path.exists());
    }

    fn hold_frame_waits(w: &mut Window, peer: &UnixStream, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while w.frame_waiters.len() != count {
            assert!(Instant::now() < deadline, "frame wait admission");
            w.control_tick();
            drain(peer);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn frame_wait_replies_share_the_two_job_budget_and_shutdown_clears_holds() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        w.event(message(SHM, 0, &[1])).unwrap();
        configure(&mut w, 800, 600);
        w.chord("C-f", false).unwrap();
        w.draw().unwrap();
        drain(&peer);
        let stamp = w.frames.capture(&w.ui).unwrap();
        let mut clients: Vec<_> = (1..=3)
            .map(|id| {
                control_client(
                    &path,
                    format!("1\t{id}\twait-frame\t{}", stamp.generation()).as_bytes(),
                )
            })
            .collect();
        hold_frame_waits(&mut w, &peer, 3);
        let state = crate::control::Request::parse(b"1\t4\tstate").unwrap();
        let before = w.control_response(&state);
        let mut query = control_client(&path, b"1\t4\tstate");
        assert_eq!(control_answer(&mut w, &mut query, &peer), before);
        assert!(w.search.is_some());
        w.event(message(WM, 0, &[918])).unwrap();
        assert!(drain(&peer)
            .0
            .iter()
            .any(|m| m.object == WM && m.opcode == 3));
        done(&mut w);
        w.control_tick();
        assert_eq!(w.frame_waiters.len(), 1);
        w.control_tick();
        assert!(w.frame_waiters.is_empty());
        for (id, client) in (1..=3).zip(&mut clients) {
            assert_eq!(
                control_answer(&mut w, client, &peer),
                format!("1\t{id}\tok\t{}", stamp.fields())
            );
        }
        w.chord("Escape", false).unwrap();
        let target = w.frames.generation().unwrap();
        let mut pending = control_client(&path, format!("1\t5\twait-frame\t{target}").as_bytes());
        hold_frame_waits(&mut w, &peer, 1);
        w.stop_control();
        assert!(w.frame_waiters.is_empty());
        assert_eq!(pending.read(&mut [0]).unwrap(), 0);
        assert!(!path.exists());
    }

    #[test]
    fn native_frame_poison_refuses_edits_drawing_and_turns_then_cleans_holds() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        let target = w.frames.generation().unwrap();
        let _client = control_client(&path, format!("1\t1\twait-frame\t{target}").as_bytes());
        hold_frame_waits(&mut w, &peer, 1);
        let before = crate::control::state(&w.ui).unwrap();
        w.frames.exhaust_for_test(); // Exercise window wiring at the counter boundary.
        assert_eq!(w.draw(), Err("exhausted".into())); // Unconfigured: no capture fallback.
        let edit = crate::control::Request::parse(b"1\t2\tinsert\t1\t0\t0\t0\t61").unwrap();
        assert!(w.control_response(&edit).contains("\terror\texhausted\t"));
        assert_eq!(crate::control::state(&w.ui).unwrap(), before);
        let result = w.end_turn(w.clock, false);
        assert_eq!(result, Err("exhausted".into()));
        assert_eq!(w.finish_control(result), Err("exhausted".into()));
        assert!(w.frame_waiters.is_empty());
        assert!(w.control.is_none());
        assert!(!path.exists());
    }

    #[test]
    fn frame_wait_deadlines_expire_without_a_callback_or_ui_mutation() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        let target = w.frames.generation().unwrap();
        let before = crate::control::state(&w.ui).unwrap();
        let mut client = control_client(&path, format!("1\t1\twait-frame\t{target}").as_bytes());
        hold_frame_waits(&mut w, &peer, 1);
        let deadline = Instant::now() + Duration::from_secs(8);
        while !w.frame_waiters.is_empty() {
            assert!(Instant::now() < deadline, "expired frame wait retained");
            w.control_tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        client.set_nonblocking(false).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert_eq!(client.read(&mut [0]).unwrap(), 0);
        assert_eq!(crate::control::state(&w.ui).unwrap(), before);
        assert_eq!(w.frames.generation().unwrap(), target);
        assert!(w.callback.is_none());
        w.stop_control();
    }

    #[test]
    fn native_control_socket_queries_preserve_modal_state_and_revision_checks() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        let socket = crate::control_socket::Socket::bind(&path).unwrap();
        w.control = Some(crate::control_worker::Worker::start(socket).unwrap());
        w.chord("a", false).unwrap();
        let tab = w.ui.editor().active().unwrap();
        let revision = w.ui.editor().document(tab).unwrap().revision();
        w.chord("C-f", false).unwrap();
        let state = crate::control::Request::parse(b"1\t10\tstate").unwrap();
        let before = w.control_response(&state);
        let mut client = control_client(&path, b"1\t10\tstate");
        assert_eq!(control_answer(&mut w, &mut client, &peer), before);
        assert!(w.search.is_some());
        assert_eq!(w.control_response(&state), before);
        assert_eq!(w.connection.wait, Duration::from_millis(10));

        let text = format!("1\t11\ttext\t{tab}\t{revision}\t0\t4");
        let mut client = control_client(&path, text.as_bytes());
        assert_eq!(
            control_answer(&mut w, &mut client, &peer),
            "1\t11\tok\t1\t61"
        );
        let mut client = control_client(&path, b"1\t12\tnew\textra");
        assert!(control_answer(&mut w, &mut client, &peer).starts_with("1\t12\terror\tprotocol\t"));
        assert_eq!(w.control_response(&state), before);
        w.chord("Escape", false).unwrap();
        w.chord("b", false).unwrap();
        w.chord("C-z", false).unwrap();
        let mut client = control_client(&path, text.as_bytes());
        assert!(control_answer(&mut w, &mut client, &peer)
            .starts_with("1\t11\terror\tstale-revision\t"));
        assert_eq!(w.ui.editor().document(tab).unwrap().text(), "a");

        drop(w);
        assert!(!path.exists());
        assert!(UnixStream::connect(&path).is_err());
    }

    #[test]
    fn native_control_shutdown_cancels_partial_and_nonreading_peers() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        let tab = w.ui.editor().active().unwrap();
        w.ui.dispatch(Event::Edit {
            tab,
            revision: 0,
            command: crate::model::Command::Insert("a".repeat(256 * 1024)),
        })
        .unwrap();
        let revision = w.ui.editor().document(tab).unwrap().revision();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        let mut partial = UnixStream::connect(&path).unwrap();
        partial.write_all(&[0, 0]).unwrap();
        let text = format!("1\t20\ttext\t{tab}\t{revision}\t0\t262144");
        let _nonreading = control_client(&path, text.as_bytes());
        // A later queued connection's reply witnesses an accept/dispatch pass.
        let mut sentinel = control_client(&path, b"1\t21\tstate");
        assert!(control_answer(&mut w, &mut sentinel, &peer).starts_with("1\t21\tok\t"));
        let start = Instant::now();
        drop(w);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(!path.exists());
        assert!(UnixStream::connect(&path).is_err());
    }

    #[test]
    fn native_control_disable_retains_cleanup_failure_until_exit() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, _peer) = file_dialog_fixture();
        w.control = Some(
            crate::control_worker::Worker::start(
                crate::control_socket::Socket::bind(&path).unwrap(),
            )
            .unwrap(),
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        w.disable_control("test transport failure");
        assert!(w.control.is_none());
        assert!(w.control_cleanup_error.is_some());
        w.chord("a", false).unwrap();
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "a");
        assert!(w
            .finish_control(Err("display failed".into()))
            .unwrap_err()
            .starts_with("display failed; control shutdown failed:"));
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    }

    #[test]
    fn native_control_queries_allow_wayland_dispatch_between_bounded_turns() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let (mut w, peer) = file_dialog_fixture();
        let socket = crate::control_socket::Socket::bind(&path).unwrap();
        w.control = Some(crate::control_worker::Worker::start(socket).unwrap());
        let mut clients: Vec<_> = (0..8)
            .map(|id| control_client(&path, format!("1\t{id}\tstate").as_bytes()))
            .collect();
        for (id, client) in clients.iter_mut().enumerate() {
            w.event(message(WM, 0, &[100 + id as u32])).unwrap();
            assert!(drain(&peer).0.contains(&message(WM, 3, &[100 + id as u32])));
            let response = control_answer(&mut w, client, &peer);
            assert!(response.starts_with(&format!("1\t{id}\tok\t")));
            assert!(response.contains("\tadapter=native-paths\t"));
        }
        assert_eq!(w.ui.editor().tabs().count(), 1);
        assert_eq!(w.ui.editor().document(1).unwrap().text(), "");
        w.control.take().unwrap().close().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn native_control_startup_failure_cleans_only_its_endpoint() {
        use std::os::unix::fs::PermissionsExt;
        let directory = DialogDirectory::new();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path("control");
        let dictionary = directory.path("missing-dictionary");
        let failed = file_window(FileWindowOptions {
            profile: Profile::Windows,
            paths: Vec::new(),
            dictionary: Some(dictionary.clone()),
            control: Some(path.clone()),
        });
        assert!(failed.is_err());
        assert!(!path.exists());
        assert!(!dictionary.exists());
        std::fs::write(&path, b"do not replace").unwrap();
        assert!(file_window(FileWindowOptions {
            profile: Profile::Windows,
            paths: Vec::new(),
            dictionary: Some(dictionary),
            control: Some(path.clone()),
        })
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"do not replace");
    }

    fn menu_click(w: &mut Window, group: crate::menu::Group, index: usize) {
        let header = w.ui.geometry().menu(group.index()).unwrap();
        pointer_move(w, header.x, header.y);
        pointer_button(w, true);
        pointer_button(w, false);
        let panel = w.menu.as_ref().unwrap().panel(w.ui.geometry()).unwrap();
        pointer_move(w, panel.x + 4, panel.y + index as i64 * 24 + 4);
        pointer_button(w, true);
        pointer_button(w, false);
    }

    #[test]
    fn menus_use_mouse_and_f10_without_editing_on_cancel_or_disabled_items() {
        use crate::menu::{Group, Item};
        for profile in [Profile::Windows, Profile::Emacs] {
            let (mut w, _peer) = file_dialog_fixture();
            w.ui.dispatch(Event::Profile(profile)).unwrap();
            let tab = w.ui.editor().active().unwrap();
            w.chord("x", false).unwrap();
            pointer_enter(&mut w);
            let before = format!("{:?}", w.ui.editor());
            if profile == Profile::Emacs {
                w.chord("C-x", false).unwrap();
            }
            let device = w.device.unwrap();
            key(&mut w, device, 68); // F10 through the real keymap.
            assert_eq!(w.menu.as_ref().unwrap().group, Group::File);
            assert!(!w.ui.keys().pending());
            w.chord("Return", true).unwrap();
            assert_eq!(w.ui.editor().tabs().count(), 1);
            w.chord("Escape", false).unwrap();
            assert_eq!(format!("{:?}", w.ui.editor()), before);
            menu_click(&mut w, Group::Edit, 2); // Disabled Cut.
            assert!(w.menu.is_some());
            assert_ne!(w.menu.as_ref().unwrap().selected, 2);
            assert_eq!(format!("{:?}", w.ui.editor()), before);
            // First click outside a popup only dismisses; it does not move
            // the document caret or start a drag under the old menu.
            pointer_move(&mut w, 700, 500);
            pointer_button(&mut w, true);
            pointer_button(&mut w, false);
            assert!(w.menu.is_none());
            assert_eq!(format!("{:?}", w.ui.editor()), before);
            w.open_menu(Group::Edit).unwrap();
            w.chord("Down", false).unwrap(); // Redo/Cut/Copy/Paste disabled.
            let menu = w.menu.as_ref().unwrap();
            assert_eq!(
                menu.group.items().get(menu.selected),
                Some(&Item::SelectAll)
            );
            w.chord("Return", false).unwrap();
            assert_eq!(
                w.ui.editor().document(tab).unwrap().selection(),
                crate::model::Selection {
                    anchor: 0,
                    caret: 1
                }
            );
        }
    }

    #[test]
    fn menus_change_profiles_and_format_modes_through_controller_commands() {
        use crate::menu::Group;
        let (mut w, _peer) = file_dialog_fixture();
        pointer_enter(&mut w);
        let tab = w.ui.editor().active().unwrap();
        w.ui.dispatch(Event::Edit {
            tab,
            revision: 0,
            command: crate::model::Command::Insert("one two three four five six".into()),
        })
        .unwrap();
        w.ui.dispatch(Event::Edit {
            tab,
            revision: 1,
            command: crate::model::Command::FillColumn(20),
        })
        .unwrap();
        menu_click(&mut w, Group::Edit, 7);
        assert_eq!(w.ui.keys().profile(), Profile::Emacs);
        menu_click(&mut w, Group::Format, 0);
        assert!(!w.ui.tab_view(tab).unwrap().soft_wrap);
        menu_click(&mut w, Group::Format, 1);
        assert!(w.ui.editor().document(tab).unwrap().auto_fill());
        menu_click(&mut w, Group::Format, 2);
        assert!(w.ui.editor().document(tab).unwrap().text().contains('\n'));
        menu_click(&mut w, Group::Edit, 0);
        assert_eq!(
            w.ui.editor().document(tab).unwrap().text(),
            "one two three four five six"
        );
        menu_click(&mut w, Group::Edit, 1);
        assert!(w.ui.editor().document(tab).unwrap().text().contains('\n'));
        menu_click(&mut w, Group::Edit, 6);
        assert_eq!(w.ui.keys().profile(), Profile::Windows);
        menu_click(&mut w, Group::Help, 0);
        assert!(w.notice.as_deref().unwrap().contains("experimental"));
    }

    #[test]
    fn menus_reuse_file_prompts_and_close_confirmation_without_bypassing_them() {
        use crate::menu::Group;
        let directory = DialogDirectory::new();
        let path = directory.path("menu-file");
        let (mut w, _peer) = file_dialog_fixture();
        pointer_enter(&mut w);
        w.chord("x", false).unwrap();
        menu_click(&mut w, Group::File, 2); // Untitled Save -> Save As.
        assert!(w.prompt.is_some() && w.menu.is_none());
        path_text(&mut w, &path);
        finish_file(&mut w);
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
        menu_click(&mut w, Group::File, 0);
        assert_eq!(w.ui.editor().tabs().count(), 2);
        w.chord("y", false).unwrap();
        let before = format!("{:?}", w.ui.editor());
        menu_click(&mut w, Group::File, 4);
        assert!(w.closing.is_some());
        w.chord("F10", false).unwrap();
        assert!(w.menu.is_none());
        w.chord("Escape", false).unwrap();
        assert_eq!(format!("{:?}", w.ui.editor()), before);
        menu_click(&mut w, Group::File, 5);
        assert!(w.closing.is_some());
        w.chord("C-d", false).unwrap();
        assert!(w.closed);
        assert_eq!(std::fs::read(path).unwrap(), b"x");
    }

    #[test]
    fn menus_refuse_clipping_stale_targets_and_late_file_completions_dismiss_them() {
        use crate::menu::Group;
        let (mut w, _peer) = file_dialog_fixture();
        let tab = w.ui.editor().active().unwrap();
        w.chord("x", false).unwrap();
        w.open_menu(Group::Edit).unwrap();
        w.ui.dispatch(Event::Edit {
            tab,
            revision: 1,
            command: crate::model::Command::Insert("y".into()),
        })
        .unwrap();
        let before = format!("{:?}", w.ui.editor());
        w.activate_menu(7).unwrap();
        assert!(w.menu.is_none());
        assert_eq!(w.ui.keys().profile(), Profile::Windows);
        assert_eq!(format!("{:?}", w.ui.editor()), before);
        configure(&mut w, 319, 600);
        w.open_menu(Group::File).unwrap();
        assert!(w.menu.is_none());
        assert!(w.notice.as_deref().unwrap().contains("Enlarge"));
        configure(&mut w, 800, 600);
        w.open_menu(Group::File).unwrap();
        configure(&mut w, 800, 160);
        assert!(w.menu.is_none());
        configure(&mut w, 800, 600);
        let directory = DialogDirectory::new();
        let path = directory.path("opened");
        std::fs::write(&path, b"disk").unwrap();
        w.files.as_mut().unwrap().open(path).unwrap();
        w.open_menu(Group::Format).unwrap();
        w.frames.clear_damage_for_test();
        finish_file(&mut w);
        assert!(w.menu.is_none());
        assert!(w.frames.is_dirty());
        assert_ne!(w.ui.editor().active(), Some(tab));
        assert_eq!(w.ui.editor().document(tab).unwrap().text(), "xy");
    }

    #[test]
    fn opening_and_cancelling_menus_changes_only_overlay_pixels() {
        let (mut w, peer) = file_dialog_fixture();
        w.notice = None;
        configure(&mut w, 800, 600);
        w.event(message(SHM, 0, &[1])).unwrap();
        w.draw().unwrap();
        drain(&peer);
        let pixels = w.pixels.clone();
        let before = format!("{:?}", w.ui.editor());
        done(&mut w);
        w.open_menu(crate::menu::Group::Edit).unwrap();
        w.draw().unwrap();
        drain(&peer);
        assert!(w.pixels != pixels);
        assert_eq!(format!("{:?}", w.ui.editor()), before);
        done(&mut w);
        w.chord("Escape", false).unwrap();
        w.draw().unwrap();
        drain(&peer);
        assert!(w.pixels == pixels);
        assert_eq!(format!("{:?}", w.ui.editor()), before);
    }

    #[test]
    fn keymap_replacement_dismisses_menus_and_repaints() {
        let (mut w, peer) = file_dialog_fixture();
        w.open_menu(crate::menu::Group::Edit).unwrap();
        w.frames.clear_damage_for_test();
        let device = w.device.unwrap();
        send_map(&mut w, &peer, device, &map_file());
        assert!(w.menu.is_none());
        assert!(w.frames.is_dirty());
    }

    #[test]
    fn clipped_menu_preserves_emacs_prefix_and_escape_clears_an_underlying_notice() {
        let (mut w, _peer) = file_dialog_fixture();
        w.ui.dispatch(Event::Profile(Profile::Emacs)).unwrap();
        configure(&mut w, 320, 160);
        w.chord("C-x", false).unwrap();
        w.chord("F10", false).unwrap();
        assert!(w.menu.is_none());
        assert!(w.ui.keys().pending());
        w.chord("C-s", false).unwrap();
        assert!(w.prompt.is_some());
        w.chord("Escape", false).unwrap();
        configure(&mut w, 800, 600);
        w.notify("Retained notice");
        w.chord("F10", false).unwrap();
        w.chord("F10", false).unwrap();
        assert!(w.menu.is_none() && w.notice.is_some());
        w.chord("F10", false).unwrap();
        w.chord("Escape", false).unwrap();
        assert!(w.menu.is_none() && w.notice.is_none());
    }

    #[test]
    fn menus_switch_groups_consume_wheel_and_dismiss_on_pointer_leave() {
        use crate::menu::Group;
        let (mut w, _peer) = file_dialog_fixture();
        pointer_enter(&mut w);
        let pointer = w.pointer.device.unwrap();
        w.chord("F10", false).unwrap();
        w.chord("Left", false).unwrap();
        assert_eq!(w.menu.as_ref().unwrap().group, Group::Help);
        w.chord("Right", false).unwrap();
        assert_eq!(w.menu.as_ref().unwrap().group, Group::File);
        w.chord("Right", false).unwrap();
        assert_eq!(w.menu.as_ref().unwrap().group, Group::Edit);
        let tab = w.ui.editor().active().unwrap();
        let before = w.ui.tab_view(tab).unwrap();
        w.event(message(pointer, 8, &[0, 100])).unwrap();
        w.event(message(pointer, 4, &[0, 0, 10000])).unwrap();
        w.event(message(pointer, 5, &[])).unwrap();
        assert!(w.menu.is_some());
        assert_eq!(w.ui.tab_view(tab).unwrap(), before);
        assert!(w.pointer.wheel_target.is_none());
        w.frames.clear_damage_for_test();
        w.event(message(pointer, 1, &[20, SURFACE])).unwrap();
        assert!(w.menu.is_none() && w.frames.is_dirty());
        w.frames.clear_damage_for_test();
        w.event(message(pointer, 1, &[21, SURFACE])).unwrap();
        assert!(!w.frames.is_dirty());
    }

    struct DialogDirectory(PathBuf);
    impl DialogDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "td-editor-dialog-{}-{}",
                std::process::id(),
                NEXT_FILE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self, leaf: &str) -> PathBuf {
            self.0.join(leaf)
        }
    }
    impl Drop for DialogDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn finish_file(w: &mut Window) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while w.files.as_ref().unwrap().busy() {
            assert!(Instant::now() < deadline, "file completion timeout");
            w.tick(w.clock + 1, false).unwrap();
            std::thread::yield_now();
        }
    }

    fn path_text(w: &mut Window, path: &Path) {
        for c in path.to_str().unwrap().chars() {
            w.chord(&c.to_string(), false).unwrap();
        }
        w.chord("Return", false).unwrap();
    }

    #[test]
    fn startup_dictionary_and_documents_share_worker_without_scanning_or_mutation() {
        let directory = DialogDirectory::new();
        let words = directory.path("words");
        let text = directory.path("text");
        std::fs::write(&words, b"known\nother").unwrap();
        std::fs::write(&text, b"known wrong").unwrap();
        let (ui, files, spelling) =
            prepare_files(Profile::Emacs, vec![text.clone()], Some(words.clone())).unwrap();
        assert_eq!(spelling.dictionary_entries(), Some(2));
        assert!(!spelling.running());
        assert!(spelling.view(ui.editor()).0.contains("not checked"));
        assert_eq!(files.labels().count(), 1);
        assert!(!files.busy());
        assert_eq!(ui.editor().document(1).unwrap().text(), "known wrong");
        assert!(!ui.editor().document(1).unwrap().dirty());
        assert_eq!(std::fs::read(&text).unwrap(), b"known wrong");
        assert_eq!(std::fs::read(&words).unwrap(), b"known\nother");
        let (ui, files, spelling) = prepare_files(Profile::Windows, Vec::new(), None).unwrap();
        assert_eq!(spelling.dictionary_entries(), None);
        assert!(ui.editor().active().is_some());
        assert_eq!(files.labels().count(), 0);
        std::fs::write(&words, b"bad data").unwrap();
        assert!(
            prepare_files(Profile::Windows, Vec::new(), Some(words.clone()))
                .err()
                .unwrap()
                .contains("dictionary selection unchanged")
        );
        let missing = directory.path("missing");
        assert!(file_window(FileWindowOptions {
            profile: Profile::Windows,
            paths: Vec::new(),
            dictionary: Some(missing.clone()),
            control: None,
        })
        .is_err());
        assert!(!missing.exists());
    }

    #[test]
    fn native_command_prompt_completes_and_dispatches_shared_actions() {
        let (mut w, peer) = file_dialog_fixture();
        w.ui.dispatch(Event::Profile(Profile::Emacs)).unwrap();
        let tab = w.ui.editor().active().unwrap();
        let device = w.device.unwrap();
        w.chord("M-x", true).unwrap();
        assert!(w.command.is_none());
        w.event(message(device, 4, &[0, 8, 0, 0, 0])).unwrap(); // Mod1/Alt
        key(&mut w, device, 45); // physical M-x
        w.event(message(device, 4, &[0, 0, 0, 0, 0])).unwrap();
        assert!(w.command.is_some());
        assert!(w.pointer_modal());
        w.chord("a", false).unwrap();
        w.chord("Return", false).unwrap();
        assert!(w.command_notice().unwrap().contains("Unknown/incomplete"));
        assert!(!w.ui.editor().document(tab).unwrap().auto_fill());
        w.chord("Tab", false).unwrap();
        assert!(w.command_notice().unwrap().contains("auto-fill-mode|"));
        w.chord("Return", true).unwrap();
        assert!(!w.ui.editor().document(tab).unwrap().auto_fill());
        w.chord("Return", false).unwrap();
        assert!(w.command.is_none());
        assert!(w.ui.editor().document(tab).unwrap().auto_fill());
        assert_eq!(w.ui.editor().document(tab).unwrap().revision(), 0);
        w.chord("M-x", false).unwrap();
        for chord in ["s", "Tab", "Return"] {
            w.chord(chord, false).unwrap();
        }
        assert!(w.command.is_none());
        assert!(w.number_notice().unwrap().contains("Fill Column"));
        for chord in ["4", "0", "Return"] {
            w.chord(chord, false).unwrap();
        }
        assert_eq!(w.ui.editor().document(tab).unwrap().fill_column(), 40);
        w.chord("M-x", false).unwrap();
        for chord in ["i", "Tab", "Return"] {
            w.chord(chord, false).unwrap();
        }
        assert!(w.command.is_none());
        assert!(w.notice.as_deref().unwrap().contains("no dictionary"));
        assert_eq!(w.ui.editor().document(tab).unwrap().text(), "");
        assert!(!w.ui.editor().document(tab).unwrap().dirty());
        drain(&peer);
    }

    #[test]
    fn native_replace_chord_menu_and_explicit_actions_share_undo_and_modal_guards() {
        for profile in [Profile::Windows, Profile::Emacs] {
            let (mut w, peer) = file_dialog_fixture();
            w.ui.dispatch(Event::Load(b"red red")).unwrap();
            w.ui.dispatch(Event::Profile(profile)).unwrap();
            let tab = w.ui.editor().active().unwrap();
            let device = w.device.unwrap();
            w.open_menu(crate::menu::Group::Edit).unwrap();
            let index = crate::menu::Group::Edit
                .items()
                .iter()
                .position(|item| *item == crate::menu::Item::Replace)
                .unwrap();
            w.activate_menu(index).unwrap();
            assert!(w.replace.is_some());
            w.chord("Escape", false).unwrap();
            assert!(w.replace.is_none());
            if profile == Profile::Windows {
                configure(&mut w, 320, 336);
                w.event(message(device, 4, &[0, 4, 0, 0, 0])).unwrap();
                key(&mut w, device, 35); // physical Ctrl+H
                w.event(message(device, 4, &[0, 0, 0, 0, 0])).unwrap();
            } else {
                w.chord("C-h", false).unwrap();
                assert!(w.replace.is_none());
                w.open_menu(crate::menu::Group::Edit).unwrap();
                let index = crate::menu::Group::Edit
                    .items()
                    .iter()
                    .position(|item| *item == crate::menu::Item::Replace)
                    .unwrap();
                w.activate_menu(index).unwrap();
            }
            assert!(w.replace.is_some() && w.pointer_modal());
            for chord in ["r", "e", "d", "Tab", "b", "l", "u", "e"] {
                w.chord(chord, false).unwrap();
            }
            for chord in ["Return", "M-r", "M-a", "Escape"] {
                w.chord(chord, true).unwrap();
            }
            assert_eq!(w.ui.editor().document(tab).unwrap().text(), "red red");
            assert_eq!(w.ui.editor().document(tab).unwrap().selection().caret, 0);
            key(&mut w, device, 28); // physical Return selects only.
            assert_eq!(
                w.ui.editor().document(tab).unwrap().selection().range(),
                0..3
            );
            w.event(message(device, 4, &[0, 8, 0, 0, 0])).unwrap();
            key(&mut w, device, 19); // physical Alt+R
            w.event(message(device, 4, &[0, 0, 0, 0, 0])).unwrap();
            assert_eq!(w.ui.editor().document(tab).unwrap().text(), "blue red");
            w.chord("M-a", false).unwrap();
            assert_eq!(w.ui.editor().document(tab).unwrap().text(), "blue blue");
            assert_eq!(w.ui.editor().document(tab).unwrap().history_depth(), (2, 0));
            w.chord("C-g", false).unwrap();
            assert!(w.replace.is_none());
            assert_eq!(w.ui.editor().document(tab).unwrap().text(), "blue blue");
            w.chord(
                if profile == Profile::Windows {
                    "C-z"
                } else {
                    "C-/"
                },
                false,
            )
            .unwrap();
            assert_eq!(w.ui.editor().document(tab).unwrap().text(), "blue red");
            drain(&peer);
        }
    }

    #[test]
    fn replace_pauses_retains_entry_rejects_stale_targets_and_closes_without_editing() {
        let (mut w, peer) = file_dialog_fixture();
        w.replace_request(1, 0).unwrap();
        w.chord("x", false).unwrap();
        let device = w.device.unwrap();
        w.event(message(device, 2, &[2, SURFACE])).unwrap();
        assert!(w.replace_notice().unwrap().contains("Replace paused"));
        assert!(w.replace_notice().unwrap().contains("Find > x"));
        focus(&mut w, device);
        w.ui.dispatch(Event::New).unwrap();
        w.chord("M-a", false).unwrap();
        assert!(w.replace.is_none());
        assert!(w.notice.as_deref().unwrap().contains("Replace cancelled"));
        assert!(!w.ui.editor().document(1).unwrap().dirty());
        assert!(!w.ui.editor().document(2).unwrap().dirty());
        w.replace_request(2, 0).unwrap();
        w.close();
        assert!(w.replace.is_none());
        drain(&peer);
    }

    #[test]
    fn command_modal_cancel_focus_stale_target_and_close_do_not_execute() {
        for profile in [Profile::Windows, Profile::Emacs] {
            let (mut w, peer) = file_dialog_fixture();
            w.ui.dispatch(Event::Profile(profile)).unwrap();
            w.open_menu(crate::menu::Group::Help).unwrap();
            w.activate_menu(1).unwrap();
            for chord in ["a", "Tab"] {
                w.chord(chord, false).unwrap();
            }
            let device = w.device.unwrap();
            w.event(message(device, 2, &[2, SURFACE])).unwrap();
            assert!(w.command_notice().unwrap().contains("Command paused"));
            assert!(w.command_notice().unwrap().contains("auto-fill-mode|"));
            focus(&mut w, device);
            w.chord("Escape", false).unwrap();
            assert!(w.command.is_none());
            assert!(!w.ui.editor().document(1).unwrap().auto_fill());
            w.command_request(1, 0).unwrap();
            for chord in ["a", "Tab"] {
                w.chord(chord, false).unwrap();
            }
            w.ui.dispatch(Event::New).unwrap();
            w.chord("Return", false).unwrap();
            assert!(w.command.is_none());
            assert!(w.notice.as_deref().unwrap().contains("Command cancelled"));
            assert!(!w.ui.editor().document(1).unwrap().auto_fill());
            assert!(!w.ui.editor().document(2).unwrap().auto_fill());
            w.command_request(2, 0).unwrap();
            for chord in ["a", "Tab"] {
                w.chord(chord, false).unwrap();
            }
            w.close();
            assert!(w.command.is_none());
            assert!(!w.ui.editor().document(2).unwrap().auto_fill());
            drain(&peer);
        }
    }

    #[test]
    fn named_fill_line_and_spelling_navigation_execute_through_the_prompt() {
        fn run(w: &mut Window, name: &str) {
            w.chord("M-x", false).unwrap();
            for scalar in name.chars() {
                w.chord(&scalar.to_string(), false).unwrap();
            }
            w.chord("Return", false).unwrap();
            assert!(w.command.is_none());
        }
        let (mut w, peer) = file_dialog_fixture();
        w.ui.dispatch(Event::Load(b"known\nwrong wrong")).unwrap();
        w.ui.dispatch(Event::Profile(Profile::Emacs)).unwrap();
        let tab = w.ui.editor().active().unwrap();
        run(&mut w, "fill-paragraph");
        assert_eq!(
            w.ui.editor().document(tab).unwrap().text(),
            "known wrong wrong"
        );
        run(&mut w, "goto-line");
        assert!(w.number_notice().unwrap().contains("Go To Line"));
        for chord in ["1", "Return"] {
            w.chord(chord, false).unwrap();
        }
        assert!(w.number.is_none());
        assert_eq!(w.ui.editor().document(tab).unwrap().selection().caret, 0);
        w.spelling
            .install(crate::spelling::Dictionary::parse(b"known").unwrap());
        run(&mut w, "ispell-buffer");
        w.end_turn(w.clock + 1, false).unwrap();
        for (name, range) in [
            ("next-misspelling", 6..11),
            ("next-misspelling", 12..17),
            ("previous-misspelling", 6..11),
        ] {
            run(&mut w, name);
            assert_eq!(
                w.ui.editor().document(tab).unwrap().selection().range(),
                range
            );
        }
        assert_eq!(w.ui.editor().document(tab).unwrap().history_depth(), (1, 0));
        drain(&peer);
    }

    #[test]
    fn fill_column_menu_is_modal_bounded_per_tab_and_nonediting_in_both_profiles() {
        let index = crate::menu::Group::Format
            .items()
            .iter()
            .position(|item| *item == crate::menu::Item::FillColumn)
            .unwrap();
        for profile in [Profile::Windows, Profile::Emacs] {
            let (mut w, peer) = file_dialog_fixture();
            w.ui.dispatch(Event::Profile(profile)).unwrap();
            let tab = w.ui.editor().active().unwrap();
            w.ui.dispatch(Event::New).unwrap();
            let other = w.ui.editor().active().unwrap();
            w.ui.dispatch(Event::SelectTab(tab)).unwrap();
            w.open_menu(crate::menu::Group::Format).unwrap();
            w.activate_menu(index).unwrap();
            assert!(w.number_notice().unwrap().contains("At open: 72"));
            w.chord("2", true).unwrap();
            for c in ["2", "4", "0", "0", "Return"] {
                w.chord(c, false).unwrap();
            }
            assert!(w.number.is_some());
            assert!(w.number_notice().unwrap().contains("Invalid fill column"));
            assert_eq!(w.ui.editor().document(tab).unwrap().fill_column(), 72);
            w.chord("C-u", false).unwrap();
            w.chord("4", false).unwrap();
            w.chord("0", false).unwrap();
            let device = w.device.unwrap();
            w.event(message(device, 2, &[2, SURFACE])).unwrap();
            assert!(w.number_notice().unwrap().contains("Fill Column paused"));
            focus(&mut w, device);
            w.chord("Return", false).unwrap();
            assert!(w.number.is_none());
            assert_eq!(
                w.notice.as_deref(),
                Some("Fill column set; existing text is unchanged.")
            );
            let doc = w.ui.editor().document(tab).unwrap();
            assert_eq!(doc.fill_column(), 40);
            assert_eq!(doc.revision(), 0);
            assert!(!doc.dirty());
            assert_eq!(doc.history_depth(), (0, 0));
            assert_eq!(w.ui.editor().document(other).unwrap().fill_column(), 72);
            for cancel in ["Escape", "C-g"] {
                w.open_menu(crate::menu::Group::Format).unwrap();
                w.activate_menu(index).unwrap();
                w.chord("2", false).unwrap();
                w.chord(cancel, false).unwrap();
                assert!(w.number.is_none());
                assert_eq!(w.ui.editor().document(tab).unwrap().fill_column(), 40);
            }
            drain(&peer);
        }
    }

    #[test]
    fn spelling_dictionary_menu_loads_explicitly_and_failed_replacement_keeps_marks() {
        use crate::menu::Group;
        let directory = DialogDirectory::new();
        let path = directory.path("words");
        std::fs::write(&path, b"known").unwrap();
        let (mut w, _peer) = file_dialog_fixture();
        w.ui.dispatch(Event::Load(b"known wrong")).unwrap();
        let before = format!("{:?}", w.ui.editor());
        w.chord("F7", false).unwrap();
        assert!(w.notice.as_deref().unwrap().contains("no dictionary"));
        w.open_menu(Group::Format).unwrap();
        w.activate_menu(5).unwrap();
        assert!(matches!(
            w.prompt.as_ref().unwrap().action,
            PathAction::Dictionary
        ));
        w.chord("F7", false).unwrap();
        assert!(!w.spelling.running());
        path_text(&mut w, &path);
        finish_file(&mut w);
        assert!(!w.spelling.running());
        assert!(w.spelling.view(w.ui.editor()).0.contains("not checked"));
        assert_eq!(format!("{:?}", w.ui.editor()), before);
        assert_eq!(w.files.as_ref().unwrap().labels().count(), 0);
        w.open_menu(Group::Format).unwrap();
        w.activate_menu(4).unwrap();
        w.end_turn(w.clock + 1, false).unwrap();
        assert_eq!(
            w.spelling.view(w.ui.editor()).1,
            std::slice::from_ref(&(6..11))
        );
        std::fs::write(&path, b"bad data").unwrap();
        w.open_menu(Group::Format).unwrap();
        w.activate_menu(5).unwrap();
        path_text(&mut w, &path);
        finish_file(&mut w);
        assert!(w
            .notice
            .as_deref()
            .unwrap()
            .contains("dictionary selection unchanged"));
        assert_eq!(
            w.spelling.view(w.ui.editor()).1,
            std::slice::from_ref(&(6..11))
        );
        std::fs::write(&path, b"known").unwrap();
        w.open_menu(Group::Format).unwrap();
        w.activate_menu(5).unwrap();
        path_text(&mut w, &path);
        finish_file(&mut w);
        assert!(w.spelling.view(w.ui.editor()).1.is_empty());
        assert!(!w.spelling.running());
        assert_eq!(format!("{:?}", w.ui.editor()), before);
    }

    #[test]
    fn native_f7_paints_underlines_and_navigation_never_edits_or_wraps() {
        for profile in [Profile::Windows, Profile::Emacs] {
            let (mut w, peer) = file_dialog_fixture();
            w.ui.dispatch(Event::Load(b"known wrong\nwrong")).unwrap();
            w.ui.dispatch(Event::Profile(profile)).unwrap();
            let tab = w.ui.editor().active().unwrap();
            w.spelling
                .install(crate::spelling::Dictionary::parse(b"known").unwrap());
            let device = w.device.unwrap();
            configure(&mut w, 800, 600);
            w.event(message(SHM, 0, &[1])).unwrap();
            w.chord("F7", true).unwrap();
            assert!(!w.spelling.running());
            key(&mut w, device, 65); // Physical F7 in both key profiles.
            assert!(w.spelling.running());
            assert!(w.spelling.view(w.ui.editor()).1.is_empty());
            w.end_turn(1, false).unwrap();
            assert_eq!(w.spelling.view(w.ui.editor()).1, &[6..11, 12..17]);
            w.draw().unwrap();
            drain(&peer);
            let area = w.ui.geometry().document();
            let offset = ((area.y as usize + 15) * 800 + area.x as usize + 6 * 8) * 4;
            assert_eq!(
                w.pixels.get(offset..offset + 4).unwrap(),
                (crate::render::MISSPELLED | 0xff000000).to_le_bytes()
            );
            for expected in [6..11, 12..17] {
                w.open_menu(crate::menu::Group::Format).unwrap();
                w.activate_menu(6).unwrap();
                assert_eq!(
                    w.ui.editor().document(tab).unwrap().selection().range(),
                    expected
                );
            }
            w.spelling_selection(false).unwrap();
            assert_eq!(
                w.ui.editor().document(tab).unwrap().selection().range(),
                12..17
            );
            assert!(w.notice.as_deref().unwrap().contains("does not wrap"));
            w.spelling_selection(true).unwrap();
            assert_eq!(
                w.ui.editor().document(tab).unwrap().selection().range(),
                6..11
            );
            assert_eq!(w.ui.editor().document(tab).unwrap().revision(), 0);
            assert!(!w.ui.editor().document(tab).unwrap().dirty());
            key(&mut w, device, 30); // Editing invalidates without starting work.
            assert!(w.spelling.view(w.ui.editor()).1.is_empty());
            // Editing must not automatically start a replacement scan.
            assert!(!w.spelling.running());
        }
    }

    #[test]
    fn native_spelling_is_chunked_and_escape_or_edit_cancels_without_partial_marks() {
        for cancel in ["Escape", "C-g", "x"] {
            let (mut w, _peer) = file_dialog_fixture();
            w.ui.dispatch(Event::Load("wrong ".repeat(2000).as_bytes()))
                .unwrap();
            w.spelling
                .install(crate::spelling::Dictionary::parse(b"known").unwrap());
            w.chord("F7", false).unwrap();
            for _ in 0..256 {
                w.tick(1, false).unwrap();
            }
            assert!(w.spelling.running());
            assert!(w.spelling.view(w.ui.editor()).1.is_empty());
            w.end_turn(1, false).unwrap();
            assert!(w.spelling.running());
            for previous in [false, true] {
                w.spelling_selection(previous).unwrap();
                let notice = w.notice.as_deref().unwrap();
                assert!(notice.contains("still checking"));
                assert!(!notice.contains("F7"));
                assert!(w.spelling.running());
            }
            assert!(w.spelling.view(w.ui.editor()).1.is_empty());
            assert!(w.connection.wait <= Duration::from_millis(1));
            w.chord(cancel, false).unwrap();
            w.end_turn(2, false).unwrap();
            assert!(!w.spelling.running());
            assert!(w.spelling.view(w.ui.editor()).1.is_empty());
            w.end_turn(3, false).unwrap();
            assert!(!w.spelling.running());
        }
    }

    #[test]
    fn scratch_spelling_notice_does_not_recommend_disabled_dictionary_loading() {
        let (mut w, _peer, _) = seat_fixture();
        w.chord("F7", false).unwrap();
        let notice = w.notice.as_deref().unwrap();
        assert!(notice.contains("no dictionary"));
        assert!(notice.contains("use --window"));
        assert!(!notice.contains("Format > Dictionary"));
        assert!(!w.spelling.running());
    }

    #[test]
    fn spelling_cancel_is_window_wide_while_dismissing_a_menu() {
        let (mut w, _peer) = file_dialog_fixture();
        w.ui.dispatch(Event::Load("wrong ".repeat(2000).as_bytes()))
            .unwrap();
        w.spelling
            .install(crate::spelling::Dictionary::parse(b"known").unwrap());
        w.chord("F7", false).unwrap();
        w.end_turn(1, false).unwrap();
        assert!(w.spelling.running());
        let before = format!("{:?}", w.ui.editor());
        w.open_menu(crate::menu::Group::Format).unwrap();
        w.chord("Escape", false).unwrap();
        assert!(w.menu.is_none());
        assert!(!w.spelling.running());
        assert_eq!(format!("{:?}", w.ui.editor()), before);
    }

    #[test]
    fn close_tab_save_as_can_cancel_or_complete_without_closing_other_tabs() {
        for profile in [Profile::Windows, Profile::Emacs] {
            let (mut w, peer) = file_dialog_fixture();
            w.ui.dispatch(Event::Profile(profile)).unwrap();
            let first = w.ui.editor().active().unwrap();
            w.chord("a", false).unwrap();
            let before = w.ui.editor().document(first).unwrap().selection();
            w.ui.dispatch(Event::New).unwrap();
            let other = w.ui.editor().active().unwrap();
            w.ui.dispatch(Event::SelectTab(first)).unwrap();
            let close_tab = |w: &mut Window| {
                if profile == Profile::Emacs {
                    w.chord("C-x", false).unwrap();
                    w.chord("k", false).unwrap();
                } else {
                    w.chord("C-w", false).unwrap();
                }
            };
            close_tab(&mut w);
            assert!(w.closing.is_some());
            w.chord("C-s", false).unwrap();
            assert!(w.prompt.is_some());
            assert!(w.path_notice().unwrap().contains("cancels ALL closing"));
            w.close(); // repeated WM close does not replace the close-tab flow
            assert!(w.prompt.is_some());
            w.chord("Escape", false).unwrap();
            assert!(w.closing.is_none() && w.prompt.is_none());
            assert!(w.notice.as_deref().unwrap().starts_with("Close cancelled;"));
            assert_eq!(w.ui.editor().tabs().count(), 2);
            assert_eq!(w.ui.editor().document(first).unwrap().selection(), before);
            close_tab(&mut w);
            w.chord("C-s", false).unwrap();
            let directory = DialogDirectory::new();
            let path = directory.path("saved");
            path_text(&mut w, &path);
            assert!(w.closing_save);
            w.event(message(WM, 0, &[991])).unwrap();
            assert!(drain(&peer).0.contains(&message(WM, 3, &[991])));
            configure(&mut w, 640, 480);
            w.chord("C-d", false).unwrap();
            w.chord("x", false).unwrap();
            assert_eq!(w.ui.editor().document(first).unwrap().text(), "a");
            assert!(!w.closed);
            finish_file(&mut w);
            assert_eq!(std::fs::read(path).unwrap(), b"a");
            assert!(w.ui.editor().document(first).is_err());
            assert_eq!(w.ui.editor().active(), Some(other));
            assert!(!w.closed && w.closing.is_none());
            assert!(!w.files.as_ref().unwrap().associated(first));
        }
    }

    #[test]
    fn window_close_cancel_preserves_discard_approvals_as_dirty_tabs() {
        let (mut w, _peer) = file_dialog_fixture();
        w.chord("a", false).unwrap();
        let first = w.ui.editor().active().unwrap();
        w.ui.dispatch(Event::New).unwrap();
        w.chord("b", false).unwrap();
        let second = w.ui.editor().active().unwrap();
        w.close();
        assert!(w.closing_notice().unwrap().contains("Ctrl+S: Save"));
        w.input.synchronized = false;
        assert!(w
            .closing_notice()
            .unwrap()
            .contains("tap and release Shift"));
        w.input.synchronized = true;
        w.chord("C-d", false).unwrap();
        assert!(!w.closed);
        assert_eq!(
            w.closing
                .as_ref()
                .unwrap()
                .next(w.ui.editor())
                .unwrap()
                .unwrap()
                .tab,
            first
        );
        assert!(w.ui.editor().document(second).unwrap().dirty());
        w.chord("C-d", true).unwrap();
        assert!(!w.closed);
        w.chord("C-g", false).unwrap();
        assert!(w.closing.is_none());
        assert_eq!(w.ui.editor().active(), Some(second));
        assert_eq!(w.ui.editor().tabs().count(), 2);
        assert_eq!(w.ui.editor().document(first).unwrap().text(), "a");
        assert_eq!(w.ui.editor().document(second).unwrap().text(), "b");
        w.close();
        w.chord("C-d", false).unwrap();
        w.chord("C-d", false).unwrap();
        assert!(w.closed);
    }

    #[test]
    fn file_close_never_confirms_a_clipped_question_and_survives_input_loss() {
        let (mut w, peer) = file_dialog_fixture();
        w.chord("a", false).unwrap();
        let tab = w.ui.editor().active().unwrap();
        w.close();
        for (width, height) in [(1, 1), (208, 480), (272, 159)] {
            w.ui.dispatch(Event::Resize {
                width,
                height,
                scale: 1,
            })
            .unwrap();
            assert!(w
                .closing_notice()
                .unwrap()
                .starts_with(&format!("Tab {tab}\n")));
            w.chord("C-d", false).unwrap();
            w.chord("C-s", false).unwrap();
            assert!(!w.closed && w.closing.is_some() && w.prompt.is_none());
            assert!(!w.files.as_ref().unwrap().busy());
        }
        w.ui.dispatch(Event::Resize {
            width: 272,
            height: 160,
            scale: 1,
        })
        .unwrap();
        let notice = w.closing_notice().unwrap();
        assert_eq!(notice.lines().count(), 6);
        assert!(notice.lines().all(|line| line.chars().count() <= 32));
        let seat = w.seat.unwrap();
        w.event(message(seat, 0, &[0])).unwrap();
        assert!(w.closing.is_some() && w.device.is_none());
        assert!(w.closing_notice().unwrap().contains("Input unavailable"));
        w.chord("C-d", false).unwrap();
        assert!(!w.closed);
        w.event(message(seat, 0, &[3])).unwrap();
        let device = w.device.unwrap();
        send_map(&mut w, &peer, device, &map_file());
        assert!(w.closing_notice().unwrap().contains("Input pending"));
        focus(&mut w, device);
        assert!(w.closing_notice().unwrap().contains("Ctrl+D"));
        w.chord("Escape", false).unwrap();
        assert!(w.closing.is_none() && !w.closed);
        assert_eq!(w.ui.editor().document(tab).unwrap().text(), "a");
    }

    #[test]
    fn later_save_failure_aborts_window_close_but_keeps_prior_saves() {
        let directory = DialogDirectory::new();
        let first_path = directory.path("first");
        let second_path = directory.path("second");
        std::fs::write(&first_path, b"first").unwrap();
        std::fs::write(&second_path, b"second").unwrap();
        let (mut w, _peer) = file_dialog_fixture();
        w.files
            .as_mut()
            .unwrap()
            .initial_open(&mut w.ui, first_path.clone())
            .unwrap();
        let first = w.ui.editor().active().unwrap();
        w.chord("a", false).unwrap();
        w.files
            .as_mut()
            .unwrap()
            .initial_open(&mut w.ui, second_path.clone())
            .unwrap();
        let second = w.ui.editor().active().unwrap();
        w.chord("b", false).unwrap();
        w.ui.dispatch(Event::SelectTab(first)).unwrap();
        w.close();
        w.chord("C-s", false).unwrap();
        finish_file(&mut w);
        assert!(!w.closed && w.closing.is_some());
        assert_eq!(std::fs::read(&first_path).unwrap(), b"afirst");
        assert_eq!(w.ui.editor().tabs().count(), 3);
        std::fs::write(&second_path, b"external").unwrap();
        w.chord("C-s", false).unwrap();
        finish_file(&mut w);
        assert!(!w.closed && w.closing.is_none());
        assert!(!w.ui.editor().document(first).unwrap().dirty());
        assert!(w.ui.editor().document(second).unwrap().dirty());
        assert_eq!(w.ui.editor().document(second).unwrap().text(), "bsecond");
        assert_eq!(std::fs::read(second_path).unwrap(), b"external");
        assert!(w.notice.as_ref().unwrap().contains("Disk conflict"));
        assert!(w
            .notice
            .as_ref()
            .unwrap()
            .starts_with("Close cancelled; tabs retained."));
        assert_eq!(w.ui.editor().tabs().count(), 3);
        assert!(w
            .conflict_notice()
            .unwrap()
            .contains("Close cancelled; Save failed."));
        w.chord("C-r", false).unwrap();
        w.chord("C-d", false).unwrap();
        finish_file(&mut w);
        assert!(w.closing.is_none() && !w.closed);
        assert_eq!(w.ui.editor().active(), Some(first));
        assert_eq!(w.ui.editor().document(second).unwrap().text(), "external");
        assert!(!w.ui.editor().document(second).unwrap().dirty());
        assert_eq!(w.ui.editor().document(first).unwrap().text(), "afirst");
    }

    #[test]
    fn conflict_reload_and_save_as_are_explicit_in_both_profiles() {
        for profile in [Profile::Windows, Profile::Emacs] {
            let directory = DialogDirectory::new();
            let path = directory.path("text");
            std::fs::write(&path, b"old").unwrap();
            let (mut w, peer) = file_dialog_fixture();
            w.ui.dispatch(Event::Profile(profile)).unwrap();
            w.files
                .as_mut()
                .unwrap()
                .initial_open(&mut w.ui, path.clone())
                .unwrap();
            let tab = w.ui.editor().active().unwrap();
            w.chord("a", false).unwrap();
            std::fs::write(&path, b"disk").unwrap();
            let save = |w: &mut Window| {
                if profile == Profile::Emacs {
                    w.chord("C-x", false).unwrap();
                }
                w.chord("C-s", false).unwrap();
                finish_file(w);
            };
            save(&mut w);
            assert!(w.conflict.is_some());
            let notice = w.conflict_notice().unwrap();
            assert!(notice.starts_with(&format!("Tab {tab}\n")));
            assert!(notice.lines().all(|line| line.chars().count() <= 32));
            w.chord("C-d", false).unwrap(); // not a discard question yet
            assert!(!w.files.as_ref().unwrap().busy());
            w.chord("C-r", true).unwrap();
            assert!(!w.conflict.as_ref().unwrap().needs_discard());
            w.chord("C-r", false).unwrap();
            assert!(w.conflict.as_ref().unwrap().needs_discard());
            w.chord("Escape", false).unwrap();
            assert!(w.conflict.is_none());
            assert_eq!(w.ui.editor().document(tab).unwrap().text(), "aold");
            save(&mut w);
            w.chord("C-r", false).unwrap();
            w.chord("C-d", true).unwrap();
            assert!(!w.files.as_ref().unwrap().busy());
            w.chord("C-d", false).unwrap();
            assert!(w.reloading.is_some() && w.conflict.is_none());
            w.event(message(WM, 0, &[992])).unwrap();
            assert!(drain(&peer).0.contains(&message(WM, 3, &[992])));
            configure(&mut w, 640, 480);
            w.chord("x", false).unwrap();
            assert_eq!(w.ui.editor().document(tab).unwrap().text(), "aold");
            finish_file(&mut w);
            assert!(w.reloading.is_none());
            assert_eq!(w.ui.editor().document(tab).unwrap().text(), "disk");
            assert_eq!(w.ui.editor().document(tab).unwrap().history_depth(), (0, 0));
            assert!(!w.ui.editor().document(tab).unwrap().dirty());
            w.chord("b", false).unwrap();
            save(&mut w); // the next job adopts the prepared baseline
            assert_eq!(std::fs::read(&path).unwrap(), b"bdisk");
            w.chord("c", false).unwrap();
            std::fs::write(&path, b"outside").unwrap();
            save(&mut w);
            w.chord("C-s", false).unwrap(); // modal Save As in either profile
            assert!(w.conflict.is_none() && w.prompt.is_some());
            let copy = directory.path("copy");
            path_text(&mut w, &copy);
            finish_file(&mut w);
            assert_eq!(std::fs::read(copy).unwrap(), b"bcdisk");
            assert_eq!(std::fs::read(path).unwrap(), b"outside");
        }
    }

    #[test]
    fn conflict_captions_fit_and_unready_or_stale_questions_never_confirm() {
        let (mut w, _peer) = file_dialog_fixture();
        w.chord("a", false).unwrap();
        let tab = w.ui.editor().active().unwrap();
        let target = Target { tab, revision: 1 };
        w.conflict = Some(Conflict::new(w.ui.editor(), target).unwrap());
        let caption = |w: &Window| {
            let notice = w.conflict_notice().unwrap();
            assert!(notice.lines().count() <= 6, "{notice}");
            assert!(
                notice.lines().all(|line| line.chars().count() <= 32),
                "{notice}"
            );
        };
        caption(&w);
        w.reloading = Some(Target {
            tab: u64::MAX,
            revision: 1,
        });
        caption(&w);
        w.reloading = None;
        w.input.synchronized = false;
        caption(&w);
        w.chord("C-r", false).unwrap();
        assert!(!w.conflict.as_ref().unwrap().needs_discard());
        w.input.synchronized = true;
        w.input.focused = false;
        caption(&w);
        w.chord("C-r", false).unwrap();
        assert!(!w.conflict.as_ref().unwrap().needs_discard());
        w.input.focused = true;
        let device = w.device.take();
        caption(&w);
        w.chord("C-r", false).unwrap();
        assert!(!w.conflict.as_ref().unwrap().needs_discard());
        w.device = device;
        let map = w.input.map.take();
        caption(&w);
        w.chord("C-r", false).unwrap();
        assert!(!w.conflict.as_ref().unwrap().needs_discard());
        w.input.map = map;
        w.ui.dispatch(Event::Resize {
            width: 208,
            height: 480,
            scale: 1,
        })
        .unwrap();
        caption(&w);
        w.chord("C-r", false).unwrap();
        assert!(!w.conflict.as_ref().unwrap().needs_discard());
        w.ui.dispatch(Event::Resize {
            width: 272,
            height: 160,
            scale: 1,
        })
        .unwrap();
        w.chord("C-r", false).unwrap();
        caption(&w);
        assert!(w.conflict.as_ref().unwrap().needs_discard());
        w.ui.dispatch(Event::Edit {
            tab,
            revision: 1,
            command: crate::model::Command::Insert("b".into()),
        })
        .unwrap();
        caption(&w);
        w.chord("C-d", false).unwrap();
        assert!(w.conflict.is_none() && !w.files.as_ref().unwrap().busy());
        assert_eq!(w.ui.editor().document(tab).unwrap().text(), "ab");
    }

    #[test]
    fn conflict_read_cancel_then_edits_keeps_text_and_old_baseline() {
        let directory = DialogDirectory::new();
        let path = directory.path("text");
        std::fs::write(&path, b"old").unwrap();
        let (mut w, _peer) = file_dialog_fixture();
        w.files
            .as_mut()
            .unwrap()
            .initial_open(&mut w.ui, path.clone())
            .unwrap();
        let tab = w.ui.editor().active().unwrap();
        w.chord("a", false).unwrap();
        std::fs::write(&path, b"disk").unwrap();
        w.chord("C-s", false).unwrap();
        finish_file(&mut w);
        w.ui.dispatch(Event::Resize {
            width: 208,
            height: 480,
            scale: 1,
        })
        .unwrap();
        w.chord("C-r", false).unwrap();
        assert!(!w.conflict.as_ref().unwrap().needs_discard());
        w.ui.dispatch(Event::Resize {
            width: 272,
            height: 160,
            scale: 1,
        })
        .unwrap();
        w.chord("C-r", false).unwrap();
        w.chord("C-d", false).unwrap();
        assert!(w.reloading.is_some());
        w.close();
        assert!(w.closing.is_none() && !w.closed);
        w.chord("Escape", false).unwrap();
        assert!(w.reloading.is_none());
        w.chord("b", false).unwrap();
        finish_file(&mut w);
        assert_eq!(w.ui.editor().document(tab).unwrap().text(), "abold");
        assert!(w.ui.editor().document(tab).unwrap().dirty());
        w.chord("C-s", false).unwrap();
        finish_file(&mut w);
        assert!(w.conflict.is_some());
        assert_eq!(std::fs::read(path).unwrap(), b"disk");
    }

    #[test]
    fn cancel_closing_during_save_allows_edits_without_later_auto_close() {
        let directory = DialogDirectory::new();
        let path = directory.path("new");
        let (mut w, _peer) = file_dialog_fixture();
        w.files
            .as_mut()
            .unwrap()
            .initial_open(&mut w.ui, path.clone())
            .unwrap();
        let tab = w.ui.editor().active().unwrap();
        w.close();
        w.chord("C-s", false).unwrap();
        assert!(w.closing_save);
        w.chord("C-g", false).unwrap();
        assert!(w.closing.is_none() && !w.closing_save);
        assert!(w.files.as_ref().unwrap().busy());
        w.chord("x", false).unwrap();
        finish_file(&mut w);
        assert!(!w.closed);
        assert_eq!(std::fs::read(path).unwrap(), b"");
        assert_eq!(w.ui.editor().document(tab).unwrap().text(), "x");
        assert!(w.ui.editor().document(tab).unwrap().dirty());
    }

    #[test]
    fn path_entry_is_literal_bounded_and_modal() {
        let (mut w, _peer) = fixture();
        w.ui.dispatch(Event::Focus(true)).unwrap();
        w.files = Some(crate::session::Session::start().unwrap());
        let tab = w.ui.editor().active().unwrap();
        let before = w.ui.editor().document(tab).unwrap().text().to_owned();
        w.notify("old notice");
        w.chord("C-o", false).unwrap();
        assert!(w.notice.is_none());
        for chord in ["~", "/", "$", "(", ";", "Space", "λ"] {
            w.chord(chord, false).unwrap();
        }
        assert_eq!(w.prompt.as_ref().unwrap().text, "~/$(; λ");
        w.chord("Backspace", false).unwrap();
        w.chord("C-u", false).unwrap();
        assert!(w.prompt.as_ref().unwrap().text.is_empty());
        w.chord("Return", false).unwrap();
        assert!(w.prompt.is_some());
        w.prompt.as_mut().unwrap().text = "a".repeat(4095);
        w.chord("λ", false).unwrap();
        assert_eq!(w.prompt.as_ref().unwrap().text.len(), 4095);
        w.chord("x", false).unwrap();
        w.chord("y", false).unwrap();
        assert_eq!(w.prompt.as_ref().unwrap().text.len(), 4096);
        assert!(w.prompt.as_ref().unwrap().notice().ends_with("x|"));
        w.chord("C-n", false).unwrap();
        assert_eq!(w.ui.editor().active(), Some(tab));
        w.chord("Escape", false).unwrap();
        assert!(w.notice.is_none());
        assert_eq!(w.ui.editor().document(tab).unwrap().text(), before);
        assert!(!w.files.as_ref().unwrap().busy());
    }

    #[test]
    fn file_window_requires_a_v5_seat_and_uses_its_own_title() {
        for version in [None, Some(4), Some(5)] {
            let (client, peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_millis(10)))
                .unwrap();
            let mut w = Window::new(client, std::env::temp_dir()).unwrap();
            w.files = Some(crate::session::Session::start().unwrap());
            for event in [
                global(1, "wl_compositor", 4),
                global(2, "wl_shm", 1),
                global(3, "xdg_wm_base", 1),
            ] {
                w.event(event).unwrap();
            }
            if let Some(version) = version {
                w.event(global(4, "wl_seat", version)).unwrap();
            }
            let result = w.event(message(SYNC, 0, &[0]));
            if version != Some(5) {
                assert!(result
                    .unwrap_err()
                    .contains("file window requires wl_seat v5+"));
                assert!(!w.bound);
            } else {
                result.unwrap();
                assert!(w.seat.is_some());
                let (messages, _) = drain(&peer);
                let title = messages
                    .iter()
                    .find(|m| m.object == TOPLEVEL && m.opcode == 2)
                    .unwrap();
                let mut cursor = Cursor::new(&title.payload);
                assert_eq!(
                    cursor.string().unwrap(),
                    "td-editor — experimental file window"
                );
                cursor.finish().unwrap();
            }
        }
    }

    #[test]
    fn path_entry_reports_input_loss_without_erasing_the_path() {
        let (mut w, peer, device) = seat_fixture();
        send_map(&mut w, &peer, device, &map_file());
        focus(&mut w, device);
        w.files = Some(crate::session::Session::start().unwrap());
        w.chord("C-o", false).unwrap();
        w.chord("x", false).unwrap();
        assert!(!w.path_notice().unwrap().contains("paused"));
        w.input.synchronized = false;
        assert!(w.path_notice().unwrap().contains("tap and release Shift"));
        w.input.map = None;
        assert!(w.path_notice().unwrap().contains("keyboard unavailable"));
        assert_eq!(w.prompt.as_ref().unwrap().text, "x");
        w.chord("Escape", false).unwrap();
        assert!(w.path_notice().is_none());
    }

    #[test]
    fn open_completion_never_repeats_into_the_new_active_tab() {
        for already_open in [false, true] {
            let (mut w, peer, device) = seat_fixture();
            send_map(&mut w, &peer, device, &map_file());
            focus(&mut w, device);
            w.files = Some(crate::session::Session::start().unwrap());
            let directory = std::env::temp_dir().join(format!(
                "td-editor-repeat-{}-{}",
                std::process::id(),
                NEXT_FILE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join("new");
            let old = w.ui.editor().active().unwrap();
            if already_open {
                w.files
                    .as_mut()
                    .unwrap()
                    .initial_open(&mut w.ui, path.clone())
                    .unwrap();
                w.ui.dispatch(Event::SelectTab(old)).unwrap();
            }
            w.files.as_mut().unwrap().open(path).unwrap();
            w.input.timing(1000, 0, w.clock).unwrap();
            // No event-loop tick between submission and this held physical press.
            w.event(message(device, 3, &[0, 0, 30, 1])).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while w.files.as_ref().unwrap().busy() {
                assert!(Instant::now() < deadline, "Open completion timeout");
                w.tick(w.clock + 1, true).unwrap();
                std::thread::yield_now();
            }
            let new = w.ui.editor().active().unwrap();
            assert_ne!(new, old);
            assert_eq!(w.ui.editor().document(new).unwrap().text(), "");
            w.tick(w.clock + 1000, true).unwrap();
            assert_eq!(w.ui.editor().document(new).unwrap().text(), "");
            // The held key still needs a release before a fresh press may type.
            w.event(message(device, 3, &[0, 0, 30, 1])).unwrap();
            assert_eq!(w.ui.editor().document(new).unwrap().text(), "");
            key(&mut w, device, 30);
            w.event(message(device, 3, &[0, 0, 30, 1])).unwrap();
            assert_eq!(w.ui.editor().document(new).unwrap().text(), "a");
            std::fs::remove_dir(directory).unwrap();
        }
    }

    #[test]
    fn keymap_reads_do_not_move_shared_offsets_and_refuse_bad_sources() {
        use std::io::{Seek, SeekFrom};
        let mut file = map_file();
        file.seek(SeekFrom::Start(9)).unwrap();
        let size = file.metadata().unwrap().len() as u32;
        for _ in 0..2 {
            assert!(read_keymap(file.try_clone().unwrap().into(), 1, size).is_ok());
            assert_eq!(file.stream_position().unwrap(), 9);
        }
        for (format, size) in [
            (0, size),
            (1, 0),
            (1, 1024 * 1024 + 1),
            (1, size + 1),
            (1, size - 1),
        ] {
            assert!(read_keymap(file.try_clone().unwrap().into(), format, size).is_err());
        }
        let (mut a, b) = UnixStream::pair().unwrap();
        assert!(read_keymap(b.into(), 1, 1)
            .unwrap_err()
            .contains("regular file"));
        assert_eq!(a.read(&mut [0]).unwrap(), 0);
        file.write_all_at(b"\0", 100).unwrap();
        assert!(read_keymap(file.into(), 1, size).is_err());
    }

    #[test]
    fn replacement_map_and_capability_loss_cancel_input_without_losing_text() {
        let (mut w, peer, device) = seat_fixture();
        send_map(&mut w, &peer, device, &map_file());
        focus(&mut w, device);
        w.event(message(device, 3, &[0, 0, 30, 1])).unwrap();
        let tab = w.ui.editor().active().unwrap();
        let text = w.ui.editor().document(tab).unwrap().text().to_owned();
        let invalid = backing_file(&std::env::temp_dir(), 4).unwrap();
        send_map(&mut w, &peer, device, &invalid);
        assert!(w.input.map.is_none());
        assert!(!w.ui.focused());
        w.tick(1000, true).unwrap();
        key(&mut w, device, 48);
        assert_eq!(w.ui.editor().document(tab).unwrap().text(), text);
        send_map(&mut w, &peer, device, &map_file());
        w.event(message(device, 4, &[0, 0, 0, 0, 0])).unwrap();
        key(&mut w, device, 48);
        assert_ne!(w.ui.editor().document(tab).unwrap().text(), text);
        let seat = w.seat.unwrap();
        w.event(message(seat, 0, &[1])).unwrap();
        assert_eq!(drain(&peer).0, [message(device, 0, &[])]);
        assert!(!w.ui.focused());
        assert!(w.device.is_none());
        send_map(&mut w, &peer, device, &map_file()); // in-flight retired event
        assert!(w.input.map.is_none());
        assert!(w.connection.descriptors.is_empty());
        w.event(message(seat, 0, &[3])).unwrap();
        assert_ne!(
            w.device,
            Some(device),
            "ID cannot be reused before delete_id"
        );
        w.event(message(DISPLAY, 1, &[device])).unwrap();
        assert_eq!(w.kind(device).unwrap(), Kind::Free);
    }

    #[test]
    fn dirty_close_requires_fresh_explicit_discard_and_cancel_restores_session() {
        let (mut w, peer, device) = seat_fixture();
        send_map(&mut w, &peer, device, &map_file());
        focus(&mut w, device);
        key(&mut w, device, 30);
        let tab = w.ui.editor().active().unwrap();
        let revision = w.ui.editor().document(tab).unwrap().revision();
        let before = w.ui.editor().document(tab).unwrap().text().to_owned();
        w.chord("C-s", false).unwrap();
        assert!(w.notice.as_ref().unwrap().contains("save is not connected"));
        w.chord("C-w", false).unwrap();
        assert!(w.ui.editor().document(tab).unwrap().dirty());
        w.event(message(TOPLEVEL, 1, &[])).unwrap();
        assert!(!w.closed && w.quitting);
        w.chord("C-d", true).unwrap();
        assert!(!w.closed);
        key(&mut w, device, 48); // modal blocks editing
        w.chord("Escape", false).unwrap();
        assert!(!w.quitting && !w.closed);
        let doc = w.ui.editor().document(tab).unwrap();
        assert_eq!(doc.text(), before);
        assert_eq!(doc.revision(), revision);
        w.event(message(TOPLEVEL, 1, &[])).unwrap();
        w.event(message(WM, 0, &[77])).unwrap();
        assert_eq!(drain(&peer).0, [message(WM, 3, &[77])]);
        w.event(message(device, 4, &[0, 4, 0, 0, 0])).unwrap();
        key(&mut w, device, 32); // Ctrl+D
        assert!(w.closed);
    }

    #[test]
    fn malformed_keyboard_events_do_not_mutate_state() {
        let (mut w, _peer, device) = seat_fixture();
        for event in [
            message(device, 1, &[0, SURFACE, 3076]),
            message(device, 1, &[0, SURFACE, 3]),
            message(device, 3, &[0, 0, 30, 2]),
            message(device, 3, &[0, 0, 30, 1, 999]),
            message(device, 4, &[0, 1]),
            message(device, 99, &[]),
        ] {
            let generation = w.ui.generation();
            assert!(w.event(event).is_err());
            assert_eq!(w.ui.generation(), generation);
        }
    }

    #[test]
    fn modal_cancel_preserves_selection_prefix_and_next_input() {
        use crate::model::{Command, Selection};
        for profile in [Profile::Windows, Profile::Emacs] {
            for cancel in ["Escape", "C-g"] {
                let (mut w, peer, device) = seat_fixture();
                send_map(&mut w, &peer, device, &map_file());
                focus(&mut w, device);
                w.ui.dispatch(Event::Profile(profile)).unwrap();
                key(&mut w, device, 30);
                let tab = w.ui.editor().active().unwrap();
                let revision = w.ui.editor().document(tab).unwrap().revision();
                w.ui.dispatch(Event::Edit {
                    tab,
                    revision,
                    command: Command::Select(Selection {
                        anchor: 0,
                        caret: 2,
                    }),
                })
                .unwrap();
                let original = w.ui.editor().document(tab).unwrap().text().to_owned();
                let view = w.ui.tab_view(tab).unwrap();
                let generation = w.ui.generation();
                let notice = w.notice.clone();
                w.close();
                w.chord(cancel, false).unwrap();
                assert!(!w.quitting && !w.closed);
                assert_eq!(w.ui.generation(), generation, "{profile:?}/{cancel}");
                assert_eq!(w.ui.tab_view(tab).unwrap(), view);
                assert_eq!(w.notice, notice);
                let doc = w.ui.editor().document(tab).unwrap();
                assert_eq!(
                    doc.selection(),
                    Selection {
                        anchor: 0,
                        caret: 2
                    }
                );
                assert_eq!(doc.text(), original);
                key(&mut w, device, 48);
                assert_eq!(
                    w.ui.editor().document(tab).unwrap().text(),
                    format!("b{}", &original[2..])
                );

                if profile == Profile::Emacs {
                    w.chord("C-x", false).unwrap();
                    assert!(w.ui.keys().pending());
                    w.close();
                    w.chord(cancel, false).unwrap();
                    assert!(
                        w.ui.keys().pending(),
                        "cancel consumed the preexisting prefix"
                    );
                    w.chord("C-s", false).unwrap();
                    assert!(!w.ui.keys().pending());
                    assert!(w
                        .notice
                        .as_ref()
                        .unwrap()
                        .starts_with("save is not connected"));
                }
            }
        }
    }

    #[test]
    fn seat_removal_drains_retired_events_and_keeps_scratch_text() {
        let (mut w, peer, device) = seat_fixture();
        send_map(&mut w, &peer, device, &map_file());
        focus(&mut w, device);
        key(&mut w, device, 30);
        let tab = w.ui.editor().active().unwrap();
        let text = w.ui.editor().document(tab).unwrap().text().to_owned();
        let seat = w.seat.unwrap();
        let pointer = w.pointer.device.unwrap();
        w.event(message(REGISTRY, 1, &[4])).unwrap();
        assert!(w.seat.is_none() && w.device.is_none() && !w.closed);
        assert_eq!(
            drain(&peer).0,
            [
                message(device, 0, &[]),
                message(pointer, 1, &[]),
                message(seat, 3, &[])
            ]
        );
        w.event(message(seat, 0, &[2])).unwrap();
        send_map(&mut w, &peer, device, &map_file());
        assert!(w.device.is_none() && w.input.map.is_none());
        assert_eq!(w.ui.editor().document(tab).unwrap().text(), text);
        w.event(message(DISPLAY, 1, &[device])).unwrap();
        w.event(message(DISPLAY, 1, &[seat])).unwrap();
    }

    #[test]
    fn seat_binding_skips_old_versions_and_selects_lowest_compatible_global() {
        let (client, peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let mut w = Window::new(client, std::env::temp_dir()).unwrap();
        for event in [
            global(1, "wl_compositor", 4),
            global(2, "wl_shm", 1),
            global(3, "xdg_wm_base", 1),
            global(4, "wl_seat", 4),
            global(5, "wl_seat", 6),
            global(6, "wl_seat", 7),
        ] {
            w.event(event).unwrap();
        }
        w.event(message(SYNC, 0, &[0])).unwrap();
        assert_eq!(w.required, [1, 2, 3, 5]);
        let (messages, _) = drain(&peer);
        let binding = messages.iter().rfind(|m| m.object == REGISTRY).unwrap();
        let mut cursor = Cursor::new(&binding.payload);
        assert_eq!(cursor.u32().unwrap(), 5);
        assert_eq!(cursor.string().unwrap(), "wl_seat");
        assert_eq!(cursor.u32().unwrap(), 6);
        assert_eq!(cursor.u32().unwrap(), w.seat.unwrap());
        cursor.finish().unwrap();
    }

    #[test]
    fn close_question_reports_unavailable_and_pending_input_until_restored() {
        let (mut w, peer, device) = seat_fixture();
        send_map(&mut w, &peer, device, &map_file());
        assert!(w
            .notice
            .as_ref()
            .unwrap()
            .contains("waiting for focus/modifier"));
        focus(&mut w, device);
        assert!(w.notice.as_ref().unwrap().starts_with("Keymap ready"));
        key(&mut w, device, 30);
        let tab = w.ui.editor().active().unwrap();
        let text = w.ui.editor().document(tab).unwrap().text().to_owned();
        w.close();
        assert!(w.close_notice().contains("Ctrl+D: Discard and quit"));
        let invalid = backing_file(&std::env::temp_dir(), 4).unwrap();
        send_map(&mut w, &peer, device, &invalid);
        assert!(w.close_notice().contains("keyboard unavailable"));
        assert!(w.close_notice().contains("loses ALL scratch text"));
        assert!(!w.close_notice().contains("Ctrl+D"));
        w.close();
        assert!(!w.closed);
        send_map(&mut w, &peer, device, &map_file());
        assert!(w.close_notice().contains("input not ready"));
        assert!(w
            .notice
            .as_ref()
            .unwrap()
            .contains("waiting for focus/modifier"));
        key(&mut w, device, 48);
        assert_eq!(w.ui.editor().document(tab).unwrap().text(), text);
        w.event(message(device, 4, &[0, 0, 0, 0, 0])).unwrap();
        assert!(w.notice.as_ref().unwrap().starts_with("Keymap ready"));
        assert!(w.close_notice().contains("Ctrl+D: Discard and quit"));
        key(&mut w, device, 1);
        assert!(!w.quitting && !w.closed);
        assert_eq!(w.ui.editor().document(tab).unwrap().text(), text);
    }

    #[test]
    fn descriptor_queue_overflow_and_disconnect_drop_every_owner() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut sender = Connection::new(a).unwrap();
        let mut receiver = Connection::new(b).unwrap();
        let mut endpoints = Vec::new();
        for n in 0..9 {
            let (peer, endpoint) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let file = File::from(OwnedFd::from(endpoint));
            sender.send(99, 0, Builder::new(), Some(&file)).unwrap();
            drop(file);
            if n < 8 {
                receiver.read_more().unwrap();
            } else {
                assert!(receiver.read_more().unwrap_err().contains("budget"));
            }
            endpoints.push(peer);
        }
        assert_eq!(receiver.descriptors.len(), 8);
        drop(receiver);
        for mut peer in endpoints {
            assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        }
    }

    #[test]
    fn full_loop_waits_for_rights_sent_after_the_complete_keymap_event() {
        let (client, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let worker = std::thread::spawn(move || {
            let mut w = Window::new(client, std::env::temp_dir()).unwrap();
            let result = w.run();
            (result, w)
        });
        let mut handshake = [0; 24];
        peer.read_exact(&mut handshake).unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let mut sender = Connection::new(peer.try_clone().unwrap()).unwrap();
        for event in [
            global(1, "wl_compositor", 4),
            global(2, "wl_shm", 1),
            global(3, "xdg_wm_base", 1),
            global(4, "wl_seat", 7),
            message(SYNC, 0, &[0]),
        ] {
            let mut body = Builder::new();
            for word in event.payload.as_chunks::<4>().0 {
                body.u32(u32::from_ne_bytes(*word));
            }
            sender.send(event.object, event.opcode, body, None).unwrap();
        }
        let start = Instant::now();
        let seat = loop {
            assert!(start.elapsed() < Duration::from_secs(2));
            let (messages, _) = drain(&peer);
            if let Some(id) = messages.iter().find_map(|m| {
                if m.object != REGISTRY {
                    return None;
                }
                let mut c = Cursor::new(&m.payload);
                if c.u32().unwrap() != 4 {
                    return None;
                }
                c.string().unwrap();
                c.u32().unwrap();
                Some(c.u32().unwrap())
            }) {
                break id;
            }
        };
        sender.words(seat, 0, &[2]).unwrap();
        let device = loop {
            assert!(start.elapsed() < Duration::from_secs(2));
            let (messages, _) = drain(&peer);
            if let Some(m) = messages.iter().find(|m| m.object == seat && m.opcode == 1) {
                break Cursor::new(&m.payload).u32().unwrap();
            }
        };
        let file = map_file();
        let mut map = Builder::new();
        map.u32(1);
        map.u32(file.metadata().unwrap().len() as u32);
        let bytes = map.message(device, 0).unwrap();
        for byte in bytes {
            peer.write_all(&[byte]).unwrap();
        }
        std::thread::sleep(Duration::from_millis(30));
        let mut ping = Builder::new();
        ping.u32(987);
        sender.send(WM, 0, ping, Some(&file)).unwrap();
        for event in [
            message(device, 1, &[0, SURFACE, 0]),
            message(device, 4, &[0, 0, 0, 0, 0]),
            message(device, 5, &[0, 0]),
            message(device, 3, &[0, 0, 30, 1]),
            message(device, 3, &[0, 0, 30, 0]),
            message(TOPLEVEL, 1, &[]),
            message(device, 4, &[0, 4, 0, 0, 0]),
            message(device, 3, &[0, 0, 32, 1]),
        ] {
            let mut body = Builder::new();
            for word in event.payload.as_chunks::<4>().0 {
                body.u32(u32::from_ne_bytes(*word));
            }
            sender.send(event.object, event.opcode, body, None).unwrap();
        }
        let (result, w) = worker.join().unwrap();
        result.unwrap();
        assert!(w.closed && w.quitting);
        assert!(w
            .ui
            .editor()
            .document(w.ui.editor().active().unwrap())
            .unwrap()
            .text()
            .starts_with("atd-editor"));
        assert!(w.connection.descriptors.is_empty());
        assert!(drain(&peer).0.contains(&message(WM, 3, &[987])));
    }

    #[test]
    fn wayland_environment_precedence_and_invalid_inherited_values() {
        let ep = |s: Option<&str>, d: Option<&str>, r: Option<&str>| {
            endpoint(s.map(Into::into), d.map(Into::into), r.map(Into::into))
        };
        assert_eq!(
            ep(Some("12"), Some("/ignored"), None).unwrap(),
            Endpoint::Inherited(12)
        );
        for value in ["", "-1", "+3", "0", "2", "3x", "9999999999999"] {
            assert!(ep(Some(value), Some("/valid"), None).is_err());
        }
        assert_eq!(
            ep(None, Some("/run/other"), None).unwrap(),
            Endpoint::Path("/run/other".into())
        );
        assert_eq!(
            ep(None, None, Some("/run/user/123")).unwrap(),
            Endpoint::Path("/run/user/123/wayland-0".into())
        );
        assert_eq!(
            ep(None, Some("nested/socket"), Some("/tmp/runtime")).unwrap(),
            Endpoint::Path("/tmp/runtime/nested/socket".into())
        );
        assert!(ep(None, Some("relative"), None).is_err());
        assert!(ep(None, None, Some("relative")).is_err());
        assert!(ep(None, Some(""), Some("/tmp")).is_err());
    }

    #[test]
    fn pools_cross_the_socket_unlinked_private_and_pixel_exact() {
        let (mut w, peer) = fixture();
        w.draw().unwrap();
        assert!(w.buffers.is_empty(), "no buffer before configure");
        configure(&mut w, 800, 600);
        w.draw().unwrap();
        let (messages, mut files) = drain(&peer);
        assert_eq!(files.len(), 1);
        let mut file = files.remove(0);
        let metadata = file.metadata().unwrap();
        assert_eq!(metadata.nlink(), 0);
        assert_eq!(metadata.mode() & 0o777, 0o600);
        let mut pixels = Vec::new();
        file.read_to_end(&mut pixels).unwrap();
        assert_eq!(pixels.len(), 800 * 600 * 4);
        assert_eq!(pixels, w.pixels);
        assert_eq!(&pixels[..4], &[0xcf, 0xdb, 0xe1, 0xff]);
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .contains(&[0x3f, 0x45, 0x48, 0xff]),
            "glyph ink"
        );
        let binds: Vec<_> = messages.iter().filter(|m| m.object == REGISTRY).collect();
        assert_eq!(binds.len(), 3);
        for (m, expected) in
            binds
                .iter()
                .zip([("wl_compositor", 4), ("wl_shm", 1), ("xdg_wm_base", 1)])
        {
            let mut c = Cursor::new(&m.payload);
            c.u32().unwrap();
            assert_eq!(c.string().unwrap(), expected.0);
            assert_eq!(c.u32().unwrap(), expected.1);
        }
        assert_eq!(messages.last().unwrap(), &message(SURFACE, 6, &[]));
        assert!(w.ui.editor().tabs().all(|(_, doc)| !doc.dirty()));
    }

    #[test]
    fn callback_is_not_release_and_three_busy_buffers_bound_resize_storms() {
        let (mut w, peer) = fixture();
        configure(&mut w, 400, 200);
        w.draw().unwrap();
        let first = w.buffers[0].id;
        let (_, files) = drain(&peer);
        let mut original = vec![0; 400 * 200 * 4];
        files[0].read_exact_at(&mut original, 0).unwrap();
        configure(&mut w, 500, 200);
        w.draw().unwrap();
        assert_eq!(w.buffers.len(), 1, "callback throttles");
        done(&mut w);
        w.draw().unwrap();
        assert_eq!(w.buffers.len(), 2);
        drain(&peer);
        done(&mut w);
        configure(&mut w, 600, 200);
        w.draw().unwrap();
        assert_eq!(w.buffers.len(), 3);
        drain(&peer);
        done(&mut w);
        configure(&mut w, 700, 200);
        configure(&mut w, 0, 240);
        w.draw().unwrap();
        assert!(w.frames.is_dirty());
        assert!(w.callback.is_none());
        assert_eq!(w.buffers.len(), 3);
        assert_eq!(w.ui.geometry().dimensions(), (700, 240));
        let mut still_original = vec![0; original.len()];
        files[0].read_exact_at(&mut still_original, 0).unwrap();
        assert_eq!(still_original, original);
        w.event(message(first, 0, &[])).unwrap();
        w.draw().unwrap();
        assert!(!w.frames.is_dirty());
        assert_eq!(w.buffers.len(), 3);
        assert_eq!(w.buffers.last().unwrap().geometry.dimensions(), (700, 240));
        assert_eq!(w.kind(first).unwrap(), Kind::RetiredBuffer);
        drain(&peer);
        w.event(message(DISPLAY, 1, &[first])).unwrap();
        assert_eq!(w.kind(first).unwrap(), Kind::Free);
    }

    #[test]
    fn release_before_done_still_waits_and_matching_buffer_is_reused() {
        let (mut w, peer) = fixture();
        configure(&mut w, 100, 100);
        w.draw().unwrap();
        drain(&peer);
        let id = w.buffers[0].id;
        w.event(message(id, 0, &[])).unwrap();
        configure(&mut w, 100, 100);
        w.draw().unwrap();
        assert!(w.frames.is_dirty());
        done(&mut w);
        w.draw().unwrap();
        let (_, files) = drain(&peer);
        assert!(files.is_empty());
        assert_eq!(w.buffers.len(), 1);
        assert_eq!(w.buffers[0].id, id);
    }

    #[test]
    fn invalid_events_are_errors_and_ids_wait_for_delete() {
        let (mut w, peer) = fixture();
        drain(&peer);
        assert!(w.event(message(DISPLAY, 1, &[SHM])).is_err());
        assert!(w.event(message(127, 0, &[])).is_err());
        assert!(w.event(message(u32::MAX, 0, &[])).is_err());
        assert!(w.event(message(TOPLEVEL, 0, &[u32::MAX, 1, 0])).is_err());
        assert!(w.event(message(TOPLEVEL, 0, &[1, 1, 3])).is_err());
        configure(&mut w, 1, 1);
        assert_eq!(w.ui.geometry().dimensions(), (1, 1));
        w.event(message(TOPLEVEL, 0, &[8193, 1, 0])).unwrap();
        assert!(w.event(message(XDG_SURFACE, 0, &[1])).is_err());
        assert_eq!(w.ui.geometry().dimensions(), (1, 1));
        let id = w.allocate(Kind::Frame).unwrap();
        w.set_kind(id, Kind::Retired).unwrap();
        assert_ne!(w.allocate(Kind::Frame).unwrap(), id);
        w.event(message(DISPLAY, 1, &[id])).unwrap();
        assert_eq!(w.allocate(Kind::Frame).unwrap(), id);
        while w.allocate(Kind::Frame).is_ok() {}
        assert!(w.allocate(Kind::Frame).is_err());
    }

    #[test]
    fn missing_low_version_removed_and_excessive_globals_are_named() {
        let (a, _b) = UnixStream::pair().unwrap();
        let mut w = Window::new(a, std::env::temp_dir()).unwrap();
        w.event(global(1, "wl_compositor", 3)).unwrap();
        assert!(w
            .event(message(SYNC, 0, &[0]))
            .unwrap_err()
            .contains("wl_compositor v4"));
        let (mut w, _b) = fixture();
        assert!(w
            .event(message(REGISTRY, 1, &[30]))
            .unwrap_err()
            .contains("removed"));
        assert!(w.event(global(20, "duplicate", 1)).is_err());
        for n in 1000..1124 {
            w.event(global(n, "optional", 1)).unwrap();
        }
        assert!(w.event(global(2000, "one too many", 1)).is_err());
    }

    #[test]
    fn ping_is_serviced_while_frame_waits_and_close_never_needs_discard() {
        let (mut w, peer) = fixture();
        configure(&mut w, 80, 80);
        w.draw().unwrap();
        drain(&peer);
        w.event(message(WM, 0, &[1234])).unwrap();
        let (messages, _) = drain(&peer);
        assert_eq!(messages, [message(WM, 3, &[1234])]);
        w.event(message(TOPLEVEL, 1, &[])).unwrap();
        w.frames.invalidate(true);
        w.draw().unwrap();
        assert!(w.closed);
        assert!(drain(&peer).0.is_empty());
    }

    #[test]
    fn nonblocking_idle_receive_waits_without_changing_shared_flags() {
        use std::os::fd::AsRawFd;
        let (stream, _peer) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let original = stream.try_clone().unwrap();
        let mut connection = Connection::new(stream).unwrap();
        let start = Instant::now();
        connection.read_more().unwrap();
        assert!(start.elapsed() >= Duration::from_millis(90));
        let status =
            std::fs::read_to_string(format!("/proc/self/fdinfo/{}", original.as_raw_fd())).unwrap();
        let flags = status
            .lines()
            .find_map(|line| line.strip_prefix("flags:\t"))
            .unwrap();
        assert_ne!(
            u32::from_str_radix(flags, 8).unwrap() & 0o4000,
            0,
            "shared nonblocking flag was changed"
        );
    }

    fn saturated_socket() -> (UnixStream, UnixStream, usize) {
        let (mut stream, peer) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut filled = 0;
        loop {
            match stream.write(&[0xab; 4096]) {
                Ok(n) => {
                    assert_ne!(n, 0);
                    filled += n;
                    assert!(filled <= 4 * 1024 * 1024);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        (stream, peer, filled)
    }

    #[test]
    fn temporary_write_backpressure_retries_and_startup_caps_the_deadline() {
        let (stream, mut peer, filled) = saturated_socket();
        let reader = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            let mut bytes = vec![0; filled];
            peer.read_exact(&mut bytes).unwrap();
            assert!(bytes.iter().all(|b| *b == 0xab));
            let mut request = vec![0; 12];
            peer.read_exact(&mut request).unwrap();
            assert_eq!(
                wire::take(&mut request).unwrap().unwrap(),
                message(WM, 3, &[77])
            );
        });
        let mut connection = Connection::new(stream).unwrap();
        connection.words(WM, 3, &[77]).unwrap();
        reader.join().unwrap();

        let (stream, _peer, _) = saturated_socket();
        let mut connection = Connection::new(stream).unwrap();
        connection.startup_deadline = Some(Instant::now() + Duration::from_millis(25));
        let start = Instant::now();
        assert!(connection
            .words(WM, 3, &[77])
            .unwrap_err()
            .contains("deadline"));
        assert!(start.elapsed() >= Duration::from_millis(20));
        assert!(connection
            .read_more()
            .unwrap_err()
            .contains("initial commit deadline"));
    }

    #[test]
    fn a_later_free_matching_buffer_is_preferred_over_replacing_the_first() {
        let (mut w, peer) = fixture();
        configure(&mut w, 100, 100);
        w.draw().unwrap();
        drain(&peer);
        done(&mut w);
        configure(&mut w, 200, 100);
        w.draw().unwrap();
        drain(&peer);
        done(&mut w);
        let first = w.buffers[0].id;
        let matching = w.buffers[1].id;
        w.event(message(first, 0, &[])).unwrap();
        w.event(message(matching, 0, &[])).unwrap();
        configure(&mut w, 200, 100);
        w.draw().unwrap();
        let (messages, files) = drain(&peer);
        assert!(files.is_empty());
        assert!(messages.contains(&message(SURFACE, 1, &[matching, 0, 0])));
        assert!(!w.buffers[0].busy);
        assert!(w.buffers[1].busy);
    }

    #[test]
    fn hidden_surface_waits_for_visibility_without_a_callback_deadline() {
        let (mut w, peer) = fixture();
        w.connection.startup_deadline = Some(Instant::now() + INITIAL_DEADLINE);
        configure(&mut w, 100, 100);
        w.draw().unwrap();
        drain(&peer);
        assert!(w.connection.startup_deadline.is_none());
        assert!(w.callback.is_some());
        assert_eq!(w.frames.wait(1), Ok(None));
        assert_eq!(w.connection.budget(WRITE_DEADLINE).unwrap(), WRITE_DEADLINE);
        w.event(message(WM, 0, &[9])).unwrap();
        assert_eq!(drain(&peer).0, [message(WM, 3, &[9])]);
        w.event(message(TOPLEVEL, 1, &[])).unwrap();
        assert!(w.closed);
    }

    #[test]
    fn complete_loop_accepts_split_events_and_closes_cleanly() {
        let (client, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let worker =
            std::thread::spawn(move || Window::new(client, std::env::temp_dir()).unwrap().run());
        let mut handshake = [0; 24];
        peer.read_exact(&mut handshake).unwrap();
        let mut events = Vec::new();
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
            events.extend(body.message(event.object, event.opcode).unwrap());
        }
        for chunk in events.chunks(3) {
            peer.write_all(chunk).unwrap();
        }
        // Wait for the initial empty surface commit, then close before a draw.
        let mut pending = Vec::new();
        loop {
            let mut buf = [0; 1024];
            let n = peer.read(&mut buf).unwrap();
            assert_ne!(n, 0);
            pending.extend_from_slice(&buf[..n]);
            let mut committed = false;
            while let Some(m) = wire::take(&mut pending).unwrap() {
                committed |= m.object == SURFACE && m.opcode == 6;
            }
            if committed {
                break;
            }
        }
        peer.write_all(&Builder::new().message(TOPLEVEL, 1).unwrap())
            .unwrap();
        worker.join().unwrap().unwrap();
    }

    #[test]
    #[ignore = "requires an independently launched Weston; set TD_EDITOR_TEST_WAYLAND to its absolute socket"]
    fn weston_presents_the_reference_buffer() {
        let path = std::env::var_os("TD_EDITOR_TEST_WAYLAND").expect("explicit Weston test socket");
        let mut w = Window::new(
            connect(Endpoint::Path(path.into())).unwrap(),
            std::env::temp_dir(),
        )
        .unwrap();
        w.connection.words(DISPLAY, 1, &[REGISTRY]).unwrap();
        w.connection.words(DISPLAY, 0, &[SYNC]).unwrap();
        let start = Instant::now();
        while w.frames.wait(1).unwrap().is_none() {
            assert!(
                start.elapsed() < INITIAL_DEADLINE,
                "Weston presentation timeout"
            );
            while let Some(m) = wire::take(&mut w.connection.pending).unwrap() {
                w.event(m).unwrap();
            }
            w.draw().unwrap();
            if w.frames.wait(1).unwrap().is_none() {
                w.connection.read_more().unwrap();
            }
        }
        assert!(!w.buffers.is_empty());
        assert!(!w.pixels.is_empty());
    }
}
