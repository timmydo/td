//! Wayland presentation and input, with an optional asynchronous file session.

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
                crate::sys::send_pool(&self.stream, suffix, file)
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
}

struct Buffer {
    id: u32,
    file: File,
    geometry: Geometry,
    busy: bool,
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
        let first = match ui.dispatch(Event::Load(b"td-editor scratch preview -- NO SAVE\n\nYou can type, select, undo and switch tabs with the keyboard.\nCtrl+Tab switches tabs. Emacs: M-q fills a paragraph.\n\nOpen, Save, mouse input, menus, clipboard and spelling are not connected.\nUse --keys=emacs or --keys=windows at startup.\n\nUnicode scalars: caf\xc3\xa9, na\xc3\xafve, \xce\xbb.\n\tTabs advance to eight-column stops.\n\nClosing dirty text asks for explicit discard. Do not use as $EDITOR.\nProcess termination still loses scratch text; keep nothing important here.\n")).map_err(error)? {
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
                    Kind::Retired | Kind::RetiredBuffer | Kind::RetiredKeyboard | Kind::RetiredSeat
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
                if self
                    .globals
                    .get(&id)
                    .is_some_and(|(name, _)| name == "wl_seat")
                    && self.required.contains(&id)
                {
                    self.release_keyboard()?;
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
                self.xrgb |= cursor.u32()? == 1;
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
        self.input.cancel_repeat();
        if self.files.as_ref().is_some_and(|files| files.busy()) {
            self.notify("File operation pending. Wait for completion, then close again; quitting does not cancel a write.");
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
        if let Some(device) = self.device.take() {
            self.connection.words(device, 0, &[])?;
            self.set_kind(device, Kind::RetiredKeyboard)?;
        }
        self.input = Input::default();
        self.ui.dispatch(Event::Focus(false)).map_err(error)?;
        self.dirty = true;
        Ok(())
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
            KeyboardEvent::Key(key, pressed) => match self.input.key(key, pressed) {
                Ok(Some(stroke)) => {
                    if self.chord(&stroke.chord, false)? && stroke.repeat {
                        self.input.arm(key, self.clock);
                    }
                }
                Ok(None) => {}
                Err(detail) => self.notify(detail),
            },
        }
        Ok(())
    }

    fn chord(&mut self, chord: &str, repeated: bool) -> Result<bool> {
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
                if self.files.as_ref().is_some_and(|files| files.busy()) {
                    self.notify("File operation pending; wait before closing tabs.");
                    return Ok(false);
                }
                match self.ui.dispatch(Event::Close { tab, revision }) {
                    Ok(_) => {
                        self.labels.retain(|(id, _)| *id != tab);
                        if let Some(files) = &mut self.files { files.forget(tab); }
                        if self.ui.editor().active().is_none() { self.closed = true; }
                        self.dirty = true;
                    }
                    Err(crate::Error::Dirty) => self.notify(if self.files.is_some() {
                        "Tab has unsaved edits. Save first, or close the window to explicitly discard all unsaved edits."
                    } else { "Tab has unsaved scratch text. Undo to clean, or close the window to discard all." }),
                    Err(detail) => self.notify(detail.to_string()),
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
            if self.ui.editor().active() != active {
                self.input.cancel_repeat();
            }
            self.notify(match result {
                Ok(notice) => notice,
                Err(detail) => format!("File operation failed: {detail}"),
            });
        }
        let before = self.ui.generation();
        self.ui.dispatch(Event::Tick(now)).map_err(error)?;
        self.clock = now;
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
        let mut raster =
            Raster::new(&mut self.pixels, &self.font, geometry, width * 4).map_err(error)?;
        raster
            .paint(&self.ui.scene(&labels).map_err(error)?, geometry.bounds())
            .map_err(error)?;
        let notice = if self.quitting {
            Some(close_notice)
        } else if path_notice.is_some() {
            path_notice.as_deref()
        } else {
            self.notice.as_deref()
        };
        if let Some(notice) = notice {
            paint_notice(&mut raster, geometry, notice);
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
                            return Err("keymap descriptor deadline".into());
                        }
                        Some((message, deadline))
                    }
                    None => wire::take(&mut self.connection.pending)?
                        .map(|m| (m, Instant::now() + WRITE_DEADLINE)),
                };
                let Some((message, deadline)) = next else {
                    break;
                };
                if message.opcode == 0
                    && matches!(
                        self.kind(message.object)?,
                        Kind::Keyboard | Kind::RetiredKeyboard
                    )
                    && self.connection.descriptors.is_empty()
                {
                    if Instant::now() >= deadline {
                        return Err("keymap descriptor deadline".into());
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
                            .ok_or("keymap descriptor deadline")?,
                    );
                }
                self.connection.read_more()?;
            }
        }
        Ok(())
    }
}

impl Window {
    fn path_notice(&self) -> Option<String> {
        let prompt = self.prompt.as_ref()?;
        let readiness = if self.device.is_none() || self.input.map.is_none() {
            "Path entry paused: keyboard unavailable; restore the seat/keymap.\n"
        } else if !self.input.focused || !self.input.synchronized {
            "Path entry paused: focus the editor; tap and release Shift.\n"
        } else {
            ""
        };
        Some(format!("{readiness}{}", prompt.notice()))
    }

    fn file_request(&mut self, name: &str, tab: crate::model::TabId, revision: u64) {
        let Some(files) = &mut self.files else {
            return;
        };
        self.input.cancel_repeat();
        if files.busy() {
            self.notify("File operation pending; wait before trying again.");
        } else if name == "save" && files.associated(tab) {
            let result = files.save(&self.ui, tab, revision, None);
            self.notify(match result {
                Ok(()) => "Saving snapshot; newer edits will remain unsaved.".into(),
                Err(detail) => detail,
            });
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
            "Escape" | "C-g" => return,
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
                self.notify(match result {
                    Ok(()) => "File operation pending...".into(),
                    Err(detail) => detail,
                });
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
        if self.files.is_some() {
            return if self.device.is_none() || self.input.map.is_none() {
                "Cannot confirm discard: keyboard unavailable.\nUnsaved edits remain in memory. Restore input to cancel or discard.\nTerminating the process loses unsaved edits."
            } else if !self.input.focused || !self.input.synchronized {
                "Discard pending; input not ready.\nFocus the editor; tap and release Shift.\nEscape/Ctrl+G: cancel and save first; Ctrl+D: discard ALL unsaved edits."
            } else {
                "Discard ALL unsaved edits and quit?\nEscape/Ctrl+G: Cancel (then Save each tab first)\nCtrl+D: Discard and quit\nCompleted saves are not reverted."
            };
        }
        if self.device.is_none() || self.input.map.is_none() {
            "Cannot confirm discard: keyboard unavailable.\nScratch text is retained in memory.\nRestore keyboard/map to cancel or discard.\nTerminating this process loses ALL scratch text."
        } else if !self.input.focused || !self.input.synchronized {
            "Discard pending; input not ready.\nFocus the editor; tap and release Shift.\nThen Ctrl+D: Discard ALL; Escape/Ctrl+G: Cancel.\nSave is unavailable in this scratch preview."
        } else {
            "Discard ALL unsaved scratch text?\nSave is unavailable in this preview.\nCtrl+D: Discard and quit\nEscape / Ctrl+G: Cancel"
        }
    }
}

enum KeyboardEvent {
    Map(u32, u32),
    Enter(u32, Vec<u32>),
    Leave(u32),
    Key(u32, bool),
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
            cursor.u32()?;
            cursor.u32()?; // Server timestamps have an unrelated, wrapping epoch.
            let key = cursor.u32()?;
            let state = cursor.u32()?;
            if state > 1 {
                return Err("invalid keyboard state for v5-v7".into());
            }
            KeyboardEvent::Key(key, state == 1)
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
        window.notify("Experimental file window. Open/Save/Save As work; no mouse, clipboard, spelling or recovery. Not ready for $EDITOR.");
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
        assert_eq!(drain(&b).0, [message(seat, 1, &[device])]);
        (w, b, device)
    }

    fn map_file() -> File {
        let source = include_str!("../tests/fixtures/us.xkb");
        let file = backing_file(&std::env::temp_dir(), source.len() + 1).unwrap();
        file.write_all_at(source.as_bytes(), 0).unwrap();
        file
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
            assert!(w.quitting && !w.closed);
            assert!(w
                .close_notice()
                .contains("Completed saves are not reverted"));
            w.chord("C-d", true).unwrap();
            assert!(!w.closed);
            w.chord("Escape", false).unwrap();
            assert!(!w.quitting);
            w.close();
            w.chord("C-d", false).unwrap();
            assert!(w.closed);
            assert_eq!(std::fs::read(&path).unwrap(), b"a");
            std::fs::remove_file(path).unwrap();
            std::fs::remove_dir(directory).unwrap();
        }
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
        w.event(message(REGISTRY, 1, &[4])).unwrap();
        assert!(w.seat.is_none() && w.device.is_none() && !w.closed);
        assert_eq!(
            drain(&peer).0,
            [message(device, 0, &[]), message(seat, 3, &[])]
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
