//! Wayland presentation and input, with an optional asynchronous file session.

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
    dirty: bool,
    callback: Option<u32>,
    presented: bool,
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
    closing_save: bool,
    conflict: Option<Conflict>,
    reloading: Option<Target>,
    menu: Option<crate::menu::Menu>,
    clipboard: crate::data::Clipboard,
    activation_serial: Option<u32>,
}

enum PathAction {
    Open,
    Save {
        tab: crate::model::TabId,
        revision: u64,
    },
}
struct PathPrompt {
    action: PathAction,
    text: String,
}
impl PathPrompt {
    fn notice(&self) -> String {
        let action = match self.action {
            PathAction::Open => "Open",
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
            dirty: true,
            callback: None,
            presented: false,
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
            closing_save: false,
            conflict: None,
            reloading: None,
            menu: None,
            clipboard: crate::data::Clipboard::default(),
            activation_serial: None,
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
                self.dirty = true;
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
                self.callback = None;
                self.presented = true;
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
        self.dirty = true;
    }

    fn close(&mut self) {
        self.clipboard.incoming = None;
        self.menu = None;
        self.stop_pointer();
        self.input.cancel_repeat();
        if self.files.is_some() {
            self.start_close(Scope::Window);
            return;
        }
        self.prompt = None;
        if self.ui.editor().tabs().any(|(_, doc)| doc.dirty()) {
            self.quitting = true;
            self.dirty = true;
        } else {
            self.closed = true;
        }
    }

    fn release_keyboard(&mut self) -> Result<()> {
        self.clipboard_focus_lost()?;
        self.menu = None;
        if let Some(device) = self.device.take() {
            self.connection.words(device, 0, &[])?;
            self.set_kind(device, Kind::RetiredKeyboard)?;
        }
        self.input = Input::default();
        self.ui.dispatch(Event::Focus(false)).map_err(error)?;
        self.dirty = true;
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
            || self.prompt.is_some()
            || self.closing.is_some()
            || self.conflict.is_some()
            || self.reloading.is_some()
    }

    fn stop_pointer(&mut self) {
        self.pointer.held = false;
        self.pointer.wheel = crate::pointer::Wheel::default();
        self.pointer.wheel_target = None;
        self.pointer.wheel_context = None;
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
                self.dirty |= self.menu.take().is_some();
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
                            self.dirty |= menu.selected != index;
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
                        self.dirty |= before != self.ui.generation();
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
        self.dirty |= before != self.ui.generation();
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
            self.dirty = true;
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
                self.dirty = true;
            }
            KeyboardEvent::Leave(surface) => {
                if surface != SURFACE {
                    return Err("keyboard leave for unknown surface".into());
                }
                self.input.focus(&[], false)?;
                self.clipboard_focus_lost()?;
                self.menu = None;
                self.ui.dispatch(Event::Focus(false)).map_err(error)?;
                self.dirty = true;
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
        }
        if self.quitting {
            if !repeated {
                match chord {
                    "C-d" => self.closed = true,
                    "Escape" | "C-g" => {
                        self.quitting = false;
                        self.dirty = true;
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
            self.dirty = true;
        }
        let Some(tab) = self.ui.editor().active() else {
            return Ok(false);
        };
        let revision = self.ui.editor().document(tab).map_err(error)?.revision();
        let before = self.ui.generation();
        let result = self.ui.dispatch(Event::Key {
            tab,
            revision,
            chord,
        });
        self.dirty |= self.ui.generation() != before;
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
            self.menu = None;
            if self.ui.editor().active() != active {
                self.input.cancel_repeat();
            }
            let succeeded = result.is_ok();
            self.notify(match result {
                Ok(notice) => notice,
                Err(detail) => format!("File operation failed: {detail}"),
            });
            if self.closing_save {
                self.closing_save = false;
                if succeeded {
                    self.advance_close();
                } else {
                    self.close_failed();
                }
            }
            self.reloading = None;
            if let Some(target) = self.files.as_mut().and_then(|files| files.take_conflict()) {
                self.input.cancel_repeat();
                match Conflict::new(self.ui.editor(), target) {
                    Ok(conflict) => {
                        self.stop_pointer();
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
        self.dirty |= self.ui.generation() != before;
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
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        if self.closed || !self.dirty || !self.configured || !self.xrgb || self.callback.is_some() {
            return Ok(());
        }
        let geometry = self.ui.geometry();
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
        let closing_notice = self.closing_notice();
        let conflict_notice = self.conflict_notice();
        let mut raster =
            Raster::new(&mut self.pixels, &self.font, geometry, width * 4).map_err(error)?;
        raster
            .paint(&self.ui.scene(&labels).map_err(error)?, geometry.bounds())
            .map_err(error)?;
        let notice = if self.quitting {
            Some(close_notice)
        } else if path_notice.is_some() {
            path_notice.as_deref()
        } else if closing_notice.is_some() {
            closing_notice.as_deref()
        } else if conflict_notice.is_some() {
            conflict_notice.as_deref()
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
        self.dirty = false;
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
            self.tick(now()?, processed < 256 && waiting.is_none())?;
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
        self.dirty = true;
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
                self.dirty = true;
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
            self.dirty = true;
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
            self.dirty = true;
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
                self.dirty = true;
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
        use crate::menu::Item;
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
        self.menu = None;
        self.stop_pointer();
        self.input.cancel_repeat();
        self.dirty = true;
        let event = match item {
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
                self.notify("td-editor: experimental Wayland text editor. Pure std Rust; bitmap Unifont, warm palette. UTF-8 clipboard needs data-device v3. No spelling or recovery yet. Do not use as $EDITOR. F10 opens menus.");
                return Ok(());
            }
            Item::Spell => return Ok(()),
        };
        if let Err(detail) = self.ui.dispatch(event) {
            self.notify(format!("Menu command refused: {detail}"));
        }
        Ok(())
    }

    fn close_tab(&mut self, tab: crate::model::TabId, revision: u64) {
        if self.files.is_some() {
            self.start_close(Scope::Tab { tab, revision });
            return;
        }
        match self.ui.dispatch(Event::Close { tab, revision }) {
            Ok(_) => {
                self.labels.retain(|(id, _)| *id != tab);
                self.closed |= self.ui.editor().active().is_none();
                self.dirty = true;
            }
            Err(crate::Error::Dirty) => self.notify(
                "Tab has unsaved scratch text. Undo to clean, or close the window to discard all.",
            ),
            Err(detail) => self.notify(detail.to_string()),
        }
    }

    fn start_close(&mut self, scope: Scope) {
        self.menu = None;
        self.stop_pointer();
        self.input.cancel_repeat();
        if self.closing.is_some() {
            return;
        }
        if self.reloading.is_some() || self.files.as_ref().is_some_and(|files| files.busy()) {
            self.notify(match scope {
                Scope::Tab { .. } => "File operation pending; wait before closing tabs.",
                Scope::Window => "File operation pending. Wait for completion, then close again; quitting does not cancel a write.",
            });
            return;
        }
        match Close::new(self.ui.editor(), scope) {
            Ok(close) => {
                self.prompt = None;
                self.conflict = None;
                self.notice = None;
                self.closing = Some(close);
                self.advance_close();
            }
            Err(detail) => self.notify(format!("Close refused: {detail}")),
        }
    }

    fn advance_close(&mut self) {
        if self.files.as_ref().is_some_and(|files| files.busy()) {
            return;
        }
        self.dirty = true;
        let Some(close) = self.closing.as_ref() else {
            return;
        };
        match close.next(self.ui.editor()) {
            Ok(Some(_)) => return,
            Err(detail) => {
                self.closing = None;
                self.notify(format!(
                    "Close cancelled: documents changed ({detail}); all remaining tabs retained."
                ));
                return;
            }
            Ok(None) => {}
        }
        let Some(close) = self.closing.take() else {
            return;
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
            Err(detail) => self.notify(format!("Close cancelled: {detail}")),
        }
    }

    fn close_chord(&mut self, chord: &str, repeated: bool) {
        if repeated {
            return;
        }
        if matches!(chord, "Escape" | "C-g") {
            self.closing = None;
            self.closing_save = false;
            self.notify(if self.files.as_ref().is_some_and(|f| f.busy()) {
                "Close cancelled; the pending Save will still finish."
            } else {
                "Close cancelled; tabs retained. Completed saves are not reverted."
            });
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
                self.advance_close();
                return;
            }
        };
        match chord {
            "C-d" => {
                if let Err(detail) = close.discard(self.ui.editor(), target) {
                    self.closing = None;
                    self.notify(format!("Discard refused: {detail}"));
                } else {
                    self.advance_close();
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
            self.conflict = None;
            if self.reloading.take().is_some() {
                if let Some(files) = self.files.as_mut() {
                    files.cancel_reload();
                }
                self.notify("Reload cancelled; pending read will finish without replacing text or baseline.");
            } else {
                // Keep the original file diagnostic available after dismissal.
                self.dirty = true;
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
                self.conflict = None;
                let result = self
                    .files
                    .as_mut()
                    .ok_or_else(|| "File session unavailable".to_string())
                    .and_then(|files| files.reload(&self.ui, permit));
                match result {
                    Ok(()) => {
                        self.reloading = Some(target);
                        self.dirty = true;
                    }
                    Err(detail) => self.notify(format!("Reload refused: {detail}; text retained")),
                }
            }
            Ok(None) => self.dirty = true,
            Err(detail) => {
                self.conflict = None;
                self.notify(format!("Reload refused: {detail}; text retained"));
            }
        }
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
            self.notice = None;
            self.prompt = Some(PathPrompt {
                action: if name == "open" {
                    PathAction::Open
                } else {
                    PathAction::Save { tab, revision }
                },
                text: String::new(),
            });
            self.dirty = true;
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
        self.dirty = true;
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
        self.dirty = true;
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
                self.dirty = true;
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

/// Experimental file window; ordinary $EDITOR invocation remains unavailable.
pub fn file_window(profile: Profile, paths: Vec<PathBuf>) -> io::Result<()> {
    let work = || -> Result<()> {
        if paths.len() > 64 {
            return Err("at most 64 input paths".into());
        }
        let mut files = crate::session::Session::start()?;
        let mut ui = Controller::default();
        for path in paths {
            files.initial_open(&mut ui, path)?;
        }
        if ui.editor().active().is_none() {
            ui.dispatch(Event::New).map_err(error)?;
        }
        ui.dispatch(Event::Profile(profile)).map_err(error)?;
        ui.dispatch(Event::Focus(false)).map_err(error)?;
        let endpoint = endpoint(
            std::env::var_os("WAYLAND_SOCKET"),
            std::env::var_os("WAYLAND_DISPLAY"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )?;
        let mut window = Window::new(connect(endpoint)?, std::env::temp_dir())?;
        window.ui = ui;
        window.labels.clear();
        window.files = Some(files);
        window.notify("Experimental file window. Files, mouse and menus work; UTF-8 clipboard needs data-device v3. No spelling or recovery. Not ready for $EDITOR.");
        let result = window.run();
        if result.is_err() && window.files.as_ref().is_some_and(|files| files.busy()) {
            return Err(format!("{}; file operation was pending and may have published. Verify the destination; unsaved edits are not recovered.", result.err().unwrap_or_default()));
        }
        result
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
        w.dirty = false;
        finish_file(&mut w);
        assert!(w.menu.is_none());
        assert!(w.dirty);
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
        w.dirty = false;
        let device = w.device.unwrap();
        send_map(&mut w, &peer, device, &map_file());
        assert!(w.menu.is_none());
        assert!(w.dirty);
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
        w.dirty = false;
        w.event(message(pointer, 1, &[20, SURFACE])).unwrap();
        assert!(w.menu.is_none() && w.dirty);
        w.dirty = false;
        w.event(message(pointer, 1, &[21, SURFACE])).unwrap();
        assert!(!w.dirty);
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
        assert!(w.dirty);
        assert!(w.callback.is_none());
        assert_eq!(w.buffers.len(), 3);
        assert_eq!(w.ui.geometry().dimensions(), (700, 240));
        let mut still_original = vec![0; original.len()];
        files[0].read_exact_at(&mut still_original, 0).unwrap();
        assert_eq!(still_original, original);
        w.event(message(first, 0, &[])).unwrap();
        w.draw().unwrap();
        assert!(!w.dirty);
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
        assert!(w.dirty);
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
        w.dirty = true;
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
        assert!(!w.presented);
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
        while !w.presented {
            assert!(
                start.elapsed() < INITIAL_DEADLINE,
                "Weston presentation timeout"
            );
            while let Some(m) = wire::take(&mut w.connection.pending).unwrap() {
                w.event(m).unwrap();
            }
            w.draw().unwrap();
            if !w.presented {
                w.connection.read_more().unwrap();
            }
        }
        assert!(!w.buffers.is_empty());
        assert!(!w.pixels.is_empty());
    }
}
