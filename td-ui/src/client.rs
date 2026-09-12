//! The Wayland client td-owned programs share: the object table, the
//! registry, one xdg toplevel surface with its SHM buffers and frame
//! callback, the pointer image, and the turn loop that drives a consumer's
//! `App` over the connection. Device objects (seat, keyboard, pointer,
//! clipboard) are the consumer's own for now: it names them with its `Tag`,
//! the client keeps their slots in the one table, and `handle` hands their
//! events back untouched. Errors are strings, as the transport's are.

use crate::raster::{MAX_AXIS, MAX_FRAME_BYTES};
use crate::wayland::{
    backing_file, cursor_pixels, error, Connection, Result, CURSOR_HEIGHT, CURSOR_WIDTH, IDLE_WAIT,
    WRITE_DEADLINE,
};
use crate::wire::{Builder, Cursor, Message};
use std::collections::BTreeMap;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The fixed ids every consumer publishes in this order: the display and
/// registry the server owns, the initial sync callback, the three bound
/// globals, then the surface, its xdg role and its toplevel. Dynamic ids
/// start right after; libwayland's server refuses a gap.
pub const DISPLAY: u32 = 1;
pub const REGISTRY: u32 = 2;
pub const SYNC: u32 = 3;
pub const COMPOSITOR: u32 = 4;
pub const SHM: u32 = 5;
pub const WM: u32 = 6;
pub const SURFACE: u32 = 7;
pub const XDG_SURFACE: u32 = 8;
pub const TOPLEVEL: u32 = 9;
const DYNAMIC: usize = 10;
/// The client id table: slots are reused only after `wl_display.delete_id`.
pub const OBJECTS: usize = 128;
/// At most this many live registry entries, each name at most `NAME_BYTES`.
pub const GLOBALS: usize = 128;
pub const NAME_BYTES: usize = 256;
/// At most this many backing files stay live; a busy one is never rewritten.
pub const BUFFERS: usize = 3;
/// Events processed before the turn checks redraw and close again.
pub const MESSAGES_PER_TURN: usize = 256;
/// The first buffer must be submitted this soon after the registry request.
pub const INITIAL_DEADLINE: Duration = Duration::from_secs(20);
const CURSOR_BYTES: usize = CURSOR_WIDTH * CURSOR_HEIGHT * 4;

/// A consumer's own object kinds in the shared table.
pub trait Tag: Copy + Eq + std::fmt::Debug {
    /// The object was destroyed and its slot waits for `delete_id`.
    fn retired(self) -> bool;
}

/// What a client id slot holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind<T> {
    Free,
    Fixed,
    Pool,
    Buffer,
    Frame,
    Retired,
    RetiredBuffer,
    CursorSurface,
    CursorBuffer,
    App(T),
}

/// One SHM buffer over its own unlinked backing file.
pub struct Buffer {
    id: u32,
    file: File,
    width: usize,
    height: usize,
    busy: bool,
}

impl Buffer {
    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Attached and not yet released by the compositor.
    pub fn busy(&self) -> bool {
        self.busy
    }
}

struct CursorImage {
    surface: u32,
    buffer: u32,
    busy: bool,
}

/// What `Client::handle` did with an event, and what the consumer does next.
#[derive(Debug, Eq, PartialEq)]
pub enum Handled {
    /// Consumed by the client; nothing to do.
    Done,
    /// The initial roundtrip finished: the three globals are bound and the
    /// fixed ids published. Bind the consumer's globals, set the title and
    /// app id, then `commit` the initial surface state.
    Bound,
    /// `wl_shm.format`: the client records XRGB and ARGB; a consumer whose
    /// pointer has entered shows its cursor on ARGB (format 0).
    Format(u32),
    /// `xdg_surface.configure`: apply the pending toplevel size, where a
    /// zero axis keeps the current one, then `acknowledge` the serial.
    Configure {
        size: Option<(i32, i32)>,
        serial: u32,
    },
    /// `xdg_toplevel.close`: `close` now, or after asking.
    CloseRequested,
    /// The frame callback of the last `present` fired.
    FrameDone,
    /// A registry global went away. A required one is still known to the
    /// client: `forget_global` to recover, or return the error.
    GlobalRemoved { id: u32, required: bool },
    /// The object is the consumer's (`Kind::App`); dispatch it yourself.
    Unhandled,
}

/// One display connection with its object table, registry, surface,
/// buffers and pointer image.
pub struct Client<T: Tag> {
    connection: Connection,
    globals: BTreeMap<u32, (String, u32)>,
    required: Vec<u32>,
    objects: [Kind<T>; OBJECTS],
    buffers: Vec<Buffer>,
    pixels: Vec<u8>,
    configured: bool,
    pending_size: Option<(i32, i32)>,
    xrgb: bool,
    argb: bool,
    cursor_image: Option<CursorImage>,
    callback: Option<u32>,
    bound: bool,
    closed: bool,
    temporary: PathBuf,
}

impl<T: Tag> Client<T> {
    /// Pool files are created in `temporary`, unlinked before use.
    pub fn new(stream: UnixStream, temporary: PathBuf) -> Result<Self> {
        let mut objects = [Kind::Free; OBJECTS];
        objects
            .get_mut(..DYNAMIC)
            .ok_or("object table")?
            .fill(Kind::Fixed);
        Ok(Self {
            connection: Connection::new(stream)?,
            globals: BTreeMap::new(),
            required: Vec::new(),
            objects,
            buffers: Vec::with_capacity(BUFFERS),
            pixels: Vec::new(),
            configured: false,
            pending_size: None,
            xrgb: false,
            argb: false,
            cursor_image: None,
            callback: None,
            bound: false,
            closed: false,
            temporary,
        })
    }

    /// The transport beneath the client, for the schedule inputs and the
    /// requests the client has no method for.
    pub fn connection(&mut self) -> &mut Connection {
        &mut self.connection
    }

    /// Sends a request whose arguments are all words.
    pub fn words(&mut self, object: u32, opcode: u16, words: &[u32]) -> Result<()> {
        self.connection.words(object, opcode, words)
    }

    /// Sends a built request, with at most one right.
    pub fn send(
        &mut self,
        object: u32,
        opcode: u16,
        body: Builder,
        file: Option<&File>,
    ) -> Result<()> {
        self.connection.send(object, opcode, body, file)
    }

    /// The oldest received right not yet consumed, if any.
    pub fn pop_descriptor(&mut self) -> Option<OwnedFd> {
        self.connection.pop_descriptor()
    }

    /// Received rights waiting for their consumer.
    pub fn descriptors(&self) -> usize {
        self.connection.descriptors()
    }

    /// The first free dynamic id, given to one of the consumer's own
    /// objects.
    pub fn allocate(&mut self, tag: T) -> Result<u32> {
        self.allocate_kind(Kind::App(tag))
    }

    fn allocate_kind(&mut self, kind: Kind<T>) -> Result<u32> {
        let (id, slot) = self
            .objects
            .iter_mut()
            .enumerate()
            .skip(DYNAMIC)
            .find(|(_, k)| **k == Kind::Free)
            .ok_or("Wayland object budget (waiting for delete_id)")?;
        *slot = kind;
        u32::try_from(id).map_err(error)
    }

    /// What the slot `id` holds.
    pub fn kind(&self, id: u32) -> Result<Kind<T>> {
        self.objects
            .get(id as usize)
            .copied()
            .ok_or("unknown Wayland object".into())
    }

    /// Re-tags one of the consumer's own objects, typically as retired
    /// once its destroy request went out; a slot the client owns, or a
    /// free one, is refused, so a consumer cannot free or fix a slot.
    pub fn set_tag(&mut self, id: u32, tag: T) -> Result<()> {
        let slot = self
            .objects
            .get_mut(id as usize)
            .ok_or("unknown Wayland object")?;
        if !matches!(slot, Kind::App(_)) {
            return Err("not the consumer's Wayland object".into());
        }
        *slot = Kind::App(tag);
        Ok(())
    }

    fn set_kind(&mut self, id: u32, kind: Kind<T>) -> Result<()> {
        *self
            .objects
            .get_mut(id as usize)
            .ok_or("unknown Wayland object")? = kind;
        Ok(())
    }

    /// The id and version of the lowest-numbered global called `name`
    /// offering at least `minimum`; `bind` selects the same one.
    pub fn find_global(&self, name: &str, minimum: u32) -> Option<(u32, u32)> {
        self.globals
            .iter()
            .find(|(_, (n, v))| n == name && *v >= minimum)
            .map(|(id, (_, v))| (*id, *v))
    }

    pub fn global_name(&self, id: u32) -> Option<&str> {
        self.globals.get(&id).map(|(name, _)| name.as_str())
    }

    /// The bound globals whose removal is fatal unless the consumer
    /// recovers.
    pub fn required(&self) -> &[u32] {
        &self.required
    }

    pub fn is_required(&self, id: u32) -> bool {
        self.required.contains(&id)
    }

    /// Drops a removed required global after the consumer released what it
    /// bound through it.
    pub fn forget_global(&mut self, id: u32) {
        self.required.retain(|global| *global != id);
        self.globals.remove(&id);
    }

    /// Binds the lowest-numbered global called `name` offering at least
    /// `version` as `id`, and marks it required.
    pub fn bind(&mut self, name: &str, version: u32, id: u32) -> Result<()> {
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
        // Fill fixed ids 7..9 before any dynamic id is published.
        self.connection.words(COMPOSITOR, 0, &[SURFACE])?;
        self.connection.words(WM, 2, &[XDG_SURFACE, SURFACE])?;
        self.connection.words(XDG_SURFACE, 1, &[TOPLEVEL])?;
        self.bound = true;
        Ok(())
    }

    fn toplevel_string(&mut self, opcode: u16, value: &str) -> Result<()> {
        let mut body = Builder::new();
        body.string(value)?;
        self.connection.send(TOPLEVEL, opcode, body, None)
    }

    pub fn set_title(&mut self, title: &str) -> Result<()> {
        self.toplevel_string(2, title)
    }

    pub fn set_app_id(&mut self, app_id: &str) -> Result<()> {
        self.toplevel_string(3, app_id)
    }

    /// Commits the surface's pending state.
    pub fn commit(&mut self) -> Result<()> {
        self.connection.words(SURFACE, 6, &[])
    }

    /// Acknowledges a configure after its size was applied.
    pub fn acknowledge(&mut self, serial: u32) -> Result<()> {
        self.connection.words(XDG_SURFACE, 4, &[serial])?;
        self.configured = true;
        Ok(())
    }

    /// Past the initial roundtrip.
    pub fn bound(&self) -> bool {
        self.bound
    }

    /// At least one configure was acknowledged.
    pub fn configured(&self) -> bool {
        self.configured
    }

    /// Test support, public because a consumer's tests are another crate:
    /// back to the state before the first configure was acknowledged, so a
    /// consumer's tests can check what it refuses until then.
    pub fn unconfigure(&mut self) {
        self.configured = false;
    }

    /// `run` returns once this is set.
    pub fn closed(&self) -> bool {
        self.closed
    }

    pub fn close(&mut self) {
        self.closed = true;
    }

    /// The outstanding frame callback, if a presented frame awaits it.
    pub fn frame_callback(&self) -> Option<u32> {
        self.callback
    }

    pub fn buffers(&self) -> &[Buffer] {
        &self.buffers
    }

    /// The bytes of the last painted frame.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// The cursor surface and buffer ids once the pointer image exists.
    pub fn cursor(&self) -> Option<(u32, u32)> {
        self.cursor_image
            .as_ref()
            .map(|image| (image.surface, image.buffer))
    }

    /// Sets the pointer image for `device` at the entering `serial`, building
    /// the one immutable ARGB pool on the first call. Nothing happens until
    /// ARGB was advertised.
    pub fn show_cursor(&mut self, device: u32, serial: u32) -> Result<()> {
        if !self.argb {
            return Ok(());
        }
        if let Some(image) = &self.cursor_image {
            let surface = image.surface;
            return self.connection.words(device, 0, &[serial, surface, 0, 0]);
        }
        let file = backing_file(&self.temporary, CURSOR_BYTES)?;
        file.write_all_at(&cursor_pixels(), 0).map_err(error)?;
        let surface = self.allocate_kind(Kind::CursorSurface)?;
        let pool = self.allocate_kind(Kind::Pool)?;
        let buffer = self.allocate_kind(Kind::CursorBuffer)?;
        let (width, height, stride) = (
            CURSOR_WIDTH as u32,
            CURSOR_HEIGHT as u32,
            (CURSOR_WIDTH * 4) as u32,
        );
        self.connection.words(COMPOSITOR, 0, &[surface])?;
        let mut body = Builder::new();
        body.u32(pool);
        body.u32(CURSOR_BYTES as u32);
        self.connection.send(SHM, 0, body, Some(&file))?;
        self.connection
            .words(pool, 0, &[buffer, 0, width, height, stride, 0])?;
        self.connection.words(pool, 1, &[])?;
        self.set_kind(pool, Kind::Retired)?;
        // Establish the cursor role before publishing its first buffer.
        self.connection.words(device, 0, &[serial, surface, 0, 0])?;
        self.connection.words(surface, 1, &[buffer, 0, 0])?;
        self.connection.words(surface, 2, &[0, 0, width, height])?;
        self.connection.words(surface, 6, &[])?;
        self.cursor_image = Some(CursorImage {
            surface,
            buffer,
            busy: true,
        });
        Ok(())
    }

    /// Whether `present` can submit now: configured, XRGB advertised, no
    /// frame callback outstanding, not closed.
    pub fn can_present(&self) -> bool {
        self.configured && self.xrgb && self.callback.is_none() && !self.closed
    }

    /// Paints and submits one `width` by `height` XRGB frame when a buffer
    /// is free: a free buffer of that size is reused, otherwise a free one
    /// of another size is destroyed and replaced, and a new one is created
    /// while fewer than `BUFFERS` exist. Returns `false` without painting
    /// when nothing can be submitted, and refuses an extent the raster
    /// could not paint (a zero axis, an axis past `MAX_AXIS`, or more than
    /// `MAX_FRAME_BYTES`) before any request. The first submission clears
    /// the startup deadline.
    pub fn present(
        &mut self,
        width: usize,
        height: usize,
        paint: &mut dyn FnMut(&mut [u8]) -> Result<()>,
    ) -> Result<bool> {
        if !self.can_present() {
            return Ok(false);
        }
        if width == 0 || height == 0 || width > MAX_AXIS || height > MAX_AXIS {
            return Err("frame axis".into());
        }
        let size = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .filter(|n| *n <= MAX_FRAME_BYTES)
            .ok_or("frame size")?;
        let free = self
            .buffers
            .iter()
            .position(|b| !b.busy && (b.width, b.height) == (width, height))
            .or_else(|| self.buffers.iter().position(|b| !b.busy));
        if free.is_none() && self.buffers.len() == BUFFERS {
            return Ok(false);
        }
        let index = match free {
            Some(index)
                if self.buffers.get(index).ok_or("buffer slot")?.size() != (width, height) =>
            {
                let old = self.buffers.remove(index);
                self.connection.words(old.id, 0, &[])?;
                self.set_kind(old.id, Kind::RetiredBuffer)?;
                self.create_buffer(width, height, size)?
            }
            Some(index) => index,
            None => self.create_buffer(width, height, size)?,
        };
        self.pixels.resize(size, 0);
        paint(&mut self.pixels)?;
        let buffer = self.buffers.get(index).ok_or("buffer slot")?;
        buffer.file.write_all_at(&self.pixels, 0).map_err(error)?;
        let id = buffer.id;
        let (w, h) = (
            u32::try_from(width).map_err(error)?,
            u32::try_from(height).map_err(error)?,
        );
        let callback = self.allocate_kind(Kind::Frame)?;
        self.connection.words(XDG_SURFACE, 3, &[0, 0, w, h])?;
        self.connection.words(SURFACE, 1, &[id, 0, 0])?;
        self.connection.words(SURFACE, 9, &[0, 0, w, h])?;
        self.connection.words(SURFACE, 3, &[callback])?;
        self.connection.words(SURFACE, 6, &[])?;
        self.buffers.get_mut(index).ok_or("buffer slot")?.busy = true;
        // Occluded surfaces may receive no callback until visible. Only the
        // initial handshake and submission have a deadline.
        self.connection.set_startup_deadline(None);
        self.callback = Some(callback);
        Ok(true)
    }

    /// Creates the pool and buffer for one `width` by `height` frame of
    /// `size` bytes, which `present` validated.
    fn create_buffer(&mut self, width: usize, height: usize, size: usize) -> Result<usize> {
        let file = backing_file(&self.temporary, size)?;
        let pool = self.allocate_kind(Kind::Pool)?;
        let buffer = self.allocate_kind(Kind::Buffer)?;
        let mut body = Builder::new();
        body.u32(pool);
        body.u32(u32::try_from(size).map_err(error)?);
        self.connection.send(SHM, 0, body, Some(&file))?;
        self.connection.words(
            pool,
            0,
            &[
                buffer,
                0,
                u32::try_from(width).map_err(error)?,
                u32::try_from(height).map_err(error)?,
                u32::try_from(width * 4).map_err(error)?,
                1,
            ],
        )?;
        self.connection.words(pool, 1, &[])?;
        self.set_kind(pool, Kind::Retired)?;
        let index = self.buffers.len();
        self.buffers.push(Buffer {
            id: buffer,
            file,
            width,
            height,
            busy: false,
        });
        Ok(index)
    }

    /// Handles what is the client's in `message` and says what remains for
    /// the consumer: events for its own objects are its to dispatch. An
    /// event for an object the table does not know is an error.
    pub fn handle(&mut self, message: &Message) -> Result<Handled> {
        let mut cursor = Cursor::new(&message.payload);
        let handled = match (message.object, message.opcode) {
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
                let retired = match self.kind(id)? {
                    Kind::Retired | Kind::RetiredBuffer => true,
                    Kind::App(tag) => tag.retired(),
                    _ => false,
                };
                if !retired {
                    return Err("unexpected delete_id".into());
                }
                self.set_kind(id, Kind::Free)?;
                Handled::Done
            }
            (REGISTRY, 0) => {
                let id = cursor.u32()?;
                let name = cursor.string()?;
                let version = cursor.u32()?;
                if id == 0
                    || name.len() > NAME_BYTES
                    || name.contains('\0')
                    || version == 0
                    || self.globals.len() >= GLOBALS
                    || self.globals.contains_key(&id)
                {
                    return Err("invalid or excessive Wayland globals".into());
                }
                self.globals.insert(id, (name, version));
                Handled::Done
            }
            (REGISTRY, 1) => {
                let id = cursor.u32()?;
                let required = self.required.contains(&id);
                if !required {
                    self.globals.remove(&id);
                }
                Handled::GlobalRemoved { id, required }
            }
            (SYNC, 0) if !self.bound => {
                cursor.u32()?;
                self.set_kind(SYNC, Kind::Retired)?;
                cursor.finish()?;
                self.initialize()?;
                return Ok(Handled::Bound);
            }
            (WM, 0) if self.bound => {
                let serial = cursor.u32()?;
                self.connection.words(WM, 3, &[serial])?;
                Handled::Done
            }
            (SHM, 0) if self.bound => {
                let format = cursor.u32()?;
                self.xrgb |= format == 1;
                self.argb |= format == 0;
                Handled::Format(format)
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
                Handled::Done
            }
            (TOPLEVEL, 1) if self.bound => Handled::CloseRequested,
            (XDG_SURFACE, 0) if self.bound => {
                let serial = cursor.u32()?;
                Handled::Configure {
                    size: self.pending_size.take(),
                    serial,
                }
            }
            (SURFACE, 0 | 1) if self.bound => {
                cursor.u32()?;
                Handled::Done
            }
            (id, 0 | 1) if self.kind(id)? == Kind::CursorSurface => {
                cursor.u32()?;
                Handled::Done
            }
            (id, 0) if self.kind(id)? == Kind::CursorBuffer => {
                let image = self
                    .cursor_image
                    .as_mut()
                    .filter(|image| image.buffer == id && image.busy)
                    .ok_or("unexpected cursor buffer release")?;
                image.busy = false;
                Handled::Done
            }
            (id, 0) if self.kind(id)? == Kind::Frame && self.callback == Some(id) => {
                cursor.u32()?;
                self.callback = None;
                self.set_kind(id, Kind::Retired)?;
                Handled::FrameDone
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
                Handled::Done
            }
            (id, 0) if self.kind(id)? == Kind::RetiredBuffer => Handled::Done,
            (id, _) if matches!(self.kind(id)?, Kind::App(_)) => return Ok(Handled::Unhandled),
            _ => {
                return Err(format!(
                    "unexpected Wayland event {}:{}",
                    message.object, message.opcode
                ));
            }
        };
        cursor.finish()?;
        Ok(handled)
    }
}

/// What `run` drives: a program holding one `Client`.
pub trait App {
    type Tag: Tag;

    fn client(&mut self) -> &mut Client<Self::Tag>;

    /// Whether this event carries a right that must have arrived before it
    /// is dispatched; the complete schema is checked here, before waiting.
    fn needs_descriptor(&self, message: &Message) -> Result<bool>;

    /// The next event waits for its right; timers that would fire meanwhile
    /// are cancelled.
    fn descriptor_wait(&mut self);

    /// Before each event, with monotonic milliseconds since `run` began.
    fn tick(&mut self, now: u64) -> Result<()>;

    /// Every event: call `Client::handle` and act on its outcome.
    fn event(&mut self, message: Message) -> Result<()>;

    /// After the turn's events; `idle` when the queue drained with no event
    /// parked, so a repeat may fire.
    fn end_turn(&mut self, now: u64, idle: bool) -> Result<()>;

    /// Paint through `Client::present` when there is something to show.
    fn draw(&mut self) -> Result<()>;
}

/// The turn loop: requests the registry and the initial sync under the
/// startup deadline, then until the client is closed dispatches at most
/// `MESSAGES_PER_TURN` events (parking one whose right has not arrived, for
/// at most the write deadline), ends the turn, draws, and waits for more.
/// Every turn starts from the transport's idle wait: what `end_turn` sets
/// decides the wait, and a parked event caps it by its remaining deadline.
pub fn run<A: App>(app: &mut A) -> Result<()> {
    let started = Instant::now();
    let now = || u64::try_from(started.elapsed().as_millis()).map_err(error);
    let mut waiting: Option<(Message, Instant)> = None;
    let client = app.client();
    client
        .connection
        .set_startup_deadline(Some(Instant::now() + INITIAL_DEADLINE));
    client.connection.words(DISPLAY, 1, &[REGISTRY])?;
    client.connection.words(DISPLAY, 0, &[SYNC])?;
    while !app.client().closed {
        app.client().connection.budget(WRITE_DEADLINE)?;
        app.client().connection.set_wait(IDLE_WAIT);
        let mut processed = 0;
        while processed < MESSAGES_PER_TURN {
            let next = match waiting.take() {
                Some((message, deadline)) => {
                    if Instant::now() >= deadline {
                        return Err("Wayland descriptor deadline".into());
                    }
                    Some((message, deadline))
                }
                None => app
                    .client()
                    .connection
                    .take()?
                    .map(|m| (m, Instant::now() + WRITE_DEADLINE)),
            };
            let Some((message, deadline)) = next else {
                break;
            };
            if app.needs_descriptor(&message)? && app.client().descriptors() == 0 {
                if Instant::now() >= deadline {
                    return Err("Wayland descriptor deadline".into());
                }
                app.descriptor_wait();
                waiting = Some((message, deadline));
                break;
            }
            app.tick(now()?)?;
            app.event(message)?;
            processed += 1;
            if app.client().closed {
                break;
            }
        }
        // Queued releases and focus changes settle before a repeat can fire.
        app.end_turn(now()?, processed < MESSAGES_PER_TURN && waiting.is_none())?;
        app.draw()?;
        if !app.client().closed && processed < MESSAGES_PER_TURN {
            if let Some((_, deadline)) = &waiting {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|d| !d.is_zero())
                    .ok_or("Wayland descriptor deadline")?;
                let connection = &mut app.client().connection;
                connection.set_wait(connection.wait().min(remaining));
            }
            app.client().connection.read_more()?;
        }
    }
    Ok(())
}
