//! Read-only presentation milestone; no seat, document paths or edit input.

use crate::font::Font;
use crate::render::{Geometry, Label, Raster};
use crate::ui::{Controller, Event, Outcome};
use crate::wire::{self, Builder, Cursor, Message};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
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
        let wait = self.budget(Duration::from_millis(100))?;
        self.stream.set_read_timeout(Some(wait)).map_err(error)?;
        let start = Instant::now();
        match crate::sys::receive(&self.stream, &mut self.read) {
            Ok(0) => Err("Wayland compositor disconnected".into()),
            Ok(count) => {
                if self.pending.len().saturating_add(count) > PENDING_BYTES {
                    return Err("Wayland receive budget".into());
                }
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
}

impl Window {
    fn new(stream: UnixStream, temporary: PathBuf) -> Result<Self> {
        let mut ui = Controller::default();
        let first = match ui.dispatch(Event::Load(b"td-editor Wayland preview\n\nThis window tests bitmap presentation and resizing.\n\nRead-only fixture: keyboard and pointer input are not connected.\nOpen, Save, menus, clipboard and spelling are still being built.\n\nUnicode scalars: caf\xc3\xa9, na\xc3\xafve, \xce\xbb.\n\tTabs advance to eight-column stops.\n\nClose using your window manager, or Ctrl+C in the launching terminal.\nDo not set $EDITOR to this preview.\n")).map_err(error)? {
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
            labels: vec![(first, "Read-only preview"), (second, "Second tab")],
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
        self.connection.words(COMPOSITOR, 0, &[SURFACE])?;
        self.connection.words(WM, 2, &[XDG_SURFACE, SURFACE])?;
        self.connection.words(XDG_SURFACE, 1, &[TOPLEVEL])?;
        for (opcode, value) in [(2, "td-editor — read-only preview"), (3, "td-editor")] {
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
                if !matches!(self.kind(id)?, Kind::Retired | Kind::RetiredBuffer) {
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
                if self.required.contains(&id) {
                    return Err("required Wayland global was removed".into());
                }
                self.globals.remove(&id);
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
            (TOPLEVEL, 1) if self.bound => self.closed = true,
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
            .collect();
        Raster::new(&mut self.pixels, &self.font, geometry, width * 4)
            .map_err(error)?
            .paint(&self.ui.scene(&labels).map_err(error)?, geometry.bounds())
            .map_err(error)?;
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
        self.connection.startup_deadline = Some(Instant::now() + INITIAL_DEADLINE);
        self.connection.words(DISPLAY, 1, &[REGISTRY])?;
        self.connection.words(DISPLAY, 0, &[SYNC])?;
        while !self.closed {
            self.connection.budget(WRITE_DEADLINE)?;
            let mut processed = 0;
            while processed < 256 {
                let Some(message) = wire::take(&mut self.connection.pending)? else {
                    break;
                };
                self.event(message)?;
                processed += 1;
                if self.closed {
                    break;
                }
            }
            self.draw()?;
            if !self.closed && processed < 256 {
                self.connection.read_more()?;
            }
        }
        Ok(())
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

/// Run the explicitly read-only fixture using the normal Wayland environment.
pub fn preview() -> io::Result<()> {
    let work = || -> Result<()> {
        let endpoint = endpoint(
            std::env::var_os("WAYLAND_SOCKET"),
            std::env::var_os("WAYLAND_DISPLAY"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )?;
        let stream = connect(endpoint)?;
        Window::new(stream, std::env::temp_dir())?.run()
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
        assert_eq!(&pixels[..4], &[0xf0, 0xf0, 0xf0, 0xff]);
        assert!(
            pixels.windows(4).any(|p| p == [0x24, 0x21, 0x20, 0xff]),
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
