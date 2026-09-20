//! The Wayland client td-owned programs share: the object table, the
//! registry, one xdg toplevel surface with its SHM buffers and frame
//! callback, the seat with its keyboard and pointer, the pointer image,
//! and the turn loop that drives a consumer's `App` over the connection.
//! The clipboard's objects are the consumer's own for now: it names them
//! with its `Tag`, the client keeps their slots in the one table, and
//! `handle` hands their events back untouched. Errors are strings, as the
//! transport's are.

use crate::data::{self, DeviceEvent, Offer, SourceEvent, OFFER_LIMIT, PLAIN, UTF8};
use crate::keyboard::{Held, Keymap, Modifiers, Stroke};
use crate::pointer;
use crate::raster::{MAX_AXIS, MAX_FRAME_BYTES};
use crate::repeat::Input;
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
    Seat,
    RetiredSeat,
    Keyboard,
    RetiredKeyboard,
    Pointer,
    RetiredPointer,
    DataManager,
    DataDevice,
    RetiredDataDevice,
    DataSource,
    RetiredDataSource,
    DataSync,
    RetiredDataSync,
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

/// What the live keyboard did, after the client applied it to its `Input`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyboardEvent {
    /// A keymap arrived and its right was consumed: compiled and installed,
    /// or refused with the reason. Either way the old map is gone, repeat
    /// is cancelled and a modifier snapshot is awaited.
    Keymap(Result<()>),
    /// Keyboard focus entered the surface, installing its held keys without
    /// typing, or left it, clearing them.
    Focus(bool),
    /// The first modifier snapshot after a map and focus: presses translate
    /// from now on.
    Ready,
    /// A press translated under the map and snapshot, with its serial for
    /// activation; the consumer arms repeat for `key` when it accepted the
    /// stroke.
    Key {
        serial: u32,
        key: u32,
        stroke: Stroke,
    },
    /// A press the map refused; the map stays.
    Refused(String),
    /// The roles the modifier state holds changed while the keyboard was
    /// ready: once per change, from the snapshot after `Ready` on. The
    /// `Ready` snapshot's roles are the baseline, not reported: a consumer
    /// starts from none, and a release from there is a change. Leaving,
    /// and a new map, clear the roles without a report; `Focus(false)` and
    /// `Keymap` are the consumer's cues.
    Held(Held),
}

/// What the clipboard hands the consumer.
#[derive(Debug)]
pub enum ClipboardEvent {
    /// `wl_data_device.selection`: `selection` now names the offer to
    /// receive from, or nothing; every other offer is retired.
    Selection,
    /// `wl_data_source.send` on the live source for a supported text
    /// MIME: the consumer writes the text it offered to this right and
    /// closes it, or drops it while another send is in progress.
    Send(OwnedFd),
    /// The compositor cancelled the live source, now retired; the
    /// consumer drops the text it kept for it.
    Cancelled,
    /// The data-device manager's global went away: the source and the
    /// device are released, and the consumer ends its transfers.
    Released,
}

/// What `Client::handle` did with an event, and what the consumer does
/// next. A send's right makes this incomparable; tests match on it.
#[derive(Debug)]
pub enum Handled {
    /// Consumed by the client; nothing to do.
    Done,
    /// The initial roundtrip finished: the three globals are bound and the
    /// fixed ids published. Bind the consumer's globals, set the title and
    /// app id, then `commit` the initial surface state.
    Bound,
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
    /// `wl_seat.capabilities`: the client now holds a keyboard and a
    /// pointer as flagged, having created or released each. A consumer
    /// drops what it kept for a device that went away.
    Capabilities { keyboard: bool, pointer: bool },
    /// The bound seat's global went away: its devices and the seat are
    /// released and the global forgotten. A consumer releases what it
    /// bound through the seat.
    SeatRemoved,
    /// The live keyboard's event, already applied to the keyboard state.
    Keyboard(KeyboardEvent),
    /// The live pointer's event; enter and leave named this surface, and
    /// the pointer image follows enter serials.
    Pointer(pointer::Event),
    /// The clipboard's outcome: the seat's selection changed, the live
    /// source must send its text, the live source was cancelled, or the
    /// data device went with its manager's global.
    Clipboard(ClipboardEvent),
    /// The object is the consumer's (`Kind::App`); dispatch it yourself.
    Unhandled,
}

/// One display connection with its object table, registry, surface,
/// buffers, seat and pointer image.
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
    seat: Option<u32>,
    keyboard: Option<u32>,
    pointer: Option<u32>,
    input: Input,
    held: Held,
    enter: Option<u32>,
    data: Option<Data>,
    bound: bool,
    closed: bool,
    temporary: PathBuf,
}

/// The clipboard: one seat-bound data device over core v3, its live
/// source, the seat's selection among the server-created offers, and the
/// barriers behind which destroyed offers' ids are dropped. It outlives
/// the device's release, because a retired device's offers still arrive
/// and must be destroyed.
struct Data {
    global: u32,
    manager: u32,
    device: Option<u32>,
    source: Option<u32>,
    selection: Option<u32>,
    offers: BTreeMap<u32, Offer>,
    barriers: BTreeMap<u32, Vec<(u32, u64)>>,
    sequence: u64,
}

/// Server-created ids start here; an offer below is refused.
const SERVER_IDS: u32 = 0xff00_0000;

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
            seat: None,
            keyboard: None,
            pointer: None,
            input: Input::default(),
            held: Held::default(),
            enter: None,
            data: None,
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

    /// Whether `message` carries a right the client consumes: a keymap,
    /// or a data source's send. The complete schema is checked here,
    /// before any wait, a server offer's included; another object the
    /// table does not know is not the client's.
    pub fn needs_descriptor(&self, message: &Message) -> Result<bool> {
        if self.is_offer(message.object) {
            data::offer(message)?;
            return Ok(false);
        }
        match (self.objects.get(message.object as usize), message.opcode) {
            (Some(Kind::Keyboard | Kind::RetiredKeyboard), 0) => {
                keyboard_message(message)?;
                Ok(true)
            }
            (Some(Kind::DataSource | Kind::RetiredDataSource), 1) => {
                data::source(message)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// The seat bound on the initial roundtrip: the lowest global offering
    /// `wl_seat` v5 or newer, capped to v7. None when the registry had no
    /// such seat, and after its removal.
    pub fn seat(&self) -> Option<u32> {
        self.seat
    }

    /// The live keyboard and pointer the seat's capabilities gave.
    pub fn keyboard(&self) -> Option<u32> {
        self.keyboard
    }

    pub fn pointer(&self) -> Option<u32> {
        self.pointer
    }

    /// The serial of the pointer's enter while it is inside the surface.
    pub fn entered(&self) -> Option<u32> {
        self.enter
    }

    /// The keyboard's state: its map, focus, modifier snapshot, held keys
    /// and armed repeat.
    pub fn input(&self) -> &Input {
        &self.input
    }

    /// Test support, public because a consumer's tests are another crate:
    /// production drives the repeat through `cancel_repeat`, `arm`,
    /// `repeat` and `wait_ms` and leaves the map, focus and snapshot to
    /// `handle`.
    #[doc(hidden)]
    pub fn input_mut(&mut self) -> &mut Input {
        &mut self.input
    }

    pub fn cancel_repeat(&mut self) {
        self.input.cancel_repeat();
    }

    /// Arms repeat at `now` for `key`, whose stroke the consumer accepted.
    pub fn arm(&mut self, key: u32, now: u64) {
        self.input.arm(key, now);
    }

    /// The repeated stroke due at `now`, at most one per call.
    pub fn repeat(&mut self, now: u64) -> Result<Option<Stroke>> {
        self.input.repeat(now)
    }

    /// Milliseconds until the armed repeat is due, between 1 and 100.
    pub fn wait_ms(&self, now: u64) -> u64 {
        self.input.wait_ms(now)
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

    /// Every advertised global as `(interface, version)`, in registry-name
    /// order (a server names them as it announces them), whether or not the
    /// client bound it. For a consumer that must assert the compositor
    /// advertised an exact global set, which `find_global` cannot express:
    /// it finds one by name at a minimum version and sees neither an extra
    /// global nor an unexpected version. td-portal's private registry check
    /// is the one caller.
    pub fn globals(&self) -> impl Iterator<Item = (&str, u32)> + '_ {
        self.globals
            .values()
            .map(|(name, version)| (name.as_str(), *version))
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
        self.bind_global(global, name, version, id)?;
        self.required.push(global);
        Ok(())
    }

    /// Sends `wl_registry.bind` for `global` as `name` v`version` at `id`.
    fn bind_global(&mut self, global: u32, name: &str, version: u32, id: u32) -> Result<()> {
        let mut body = Builder::new();
        body.u32(global);
        body.string(name)?;
        body.u32(version);
        body.u32(id);
        self.connection.send(REGISTRY, 0, body, None)
    }

    fn initialize(&mut self) -> Result<()> {
        self.bind("wl_compositor", 4, COMPOSITOR)?;
        self.bind("wl_shm", 1, SHM)?;
        self.bind("xdg_wm_base", 1, WM)?;
        // Fill fixed ids 7..9 before any dynamic id is published.
        self.connection.words(COMPOSITOR, 0, &[SURFACE])?;
        self.connection.words(WM, 2, &[XDG_SURFACE, SURFACE])?;
        self.connection.words(XDG_SURFACE, 1, &[TOPLEVEL])?;
        // The lowest seat offering v5, capped to v7; a consumer that needs
        // one checks `seat` when it is told `Bound`.
        if let Some((_, version)) = self.find_global("wl_seat", 5) {
            let seat = self.allocate_kind(Kind::Seat)?;
            self.bind("wl_seat", version.min(7), seat)?;
            self.seat = Some(seat);
        }
        // One seat-bound data device over core v3, capped there and not
        // required; a manager advertised later is not bound.
        if let (Some(seat), Some((global, _))) =
            (self.seat, self.find_global("wl_data_device_manager", 3))
        {
            let manager = self.allocate_kind(Kind::DataManager)?;
            let device = self.allocate_kind(Kind::DataDevice)?;
            self.bind_global(global, "wl_data_device_manager", 3, manager)?;
            self.connection.words(manager, 1, &[device, seat])?;
            self.data = Some(Data {
                global,
                manager,
                device: Some(device),
                source: None,
                selection: None,
                offers: BTreeMap::new(),
                barriers: BTreeMap::new(),
                sequence: 0,
            });
        }
        self.bound = true;
        Ok(())
    }

    /// Creates or releases the seat's devices as `capabilities` says: the
    /// pointer first, then the keyboard, as the editor did.
    fn capabilities(&mut self, seat: u32, capabilities: u32) -> Result<Handled> {
        let pointer = capabilities & 1 != 0;
        let keyboard = capabilities & 2 != 0;
        if !pointer {
            self.release_pointer()?;
        } else if self.pointer.is_none() {
            let device = self.allocate_kind(Kind::Pointer)?;
            self.connection.words(seat, 0, &[device])?;
            self.pointer = Some(device);
        }
        if !keyboard {
            self.release_keyboard()?;
        } else if self.keyboard.is_none() {
            let device = self.allocate_kind(Kind::Keyboard)?;
            self.connection.words(seat, 1, &[device])?;
            self.keyboard = Some(device);
        }
        Ok(Handled::Capabilities { keyboard, pointer })
    }

    /// Releases the keyboard, if any, and forgets its map, focus, snapshot,
    /// held keys and repeat; the clipboard selection goes with focus.
    fn release_keyboard(&mut self) -> Result<()> {
        if let Some(device) = self.keyboard.take() {
            self.connection.words(device, 0, &[])?;
            self.set_kind(device, Kind::RetiredKeyboard)?;
        }
        self.input = Input::default();
        self.clear_selection()
    }

    /// Releases the pointer, if any; the pointer image stays for the next.
    fn release_pointer(&mut self) -> Result<()> {
        if let Some(device) = self.pointer.take() {
            self.connection.words(device, 1, &[])?;
            self.set_kind(device, Kind::RetiredPointer)?;
        }
        self.enter = None;
        Ok(())
    }

    /// Releases both devices and the data device, then the seat.
    fn release_seat(&mut self) -> Result<()> {
        self.release_keyboard()?;
        self.release_pointer()?;
        self.release_data()?;
        if let Some(seat) = self.seat.take() {
            self.connection.words(seat, 3, &[])?;
            self.set_kind(seat, Kind::RetiredSeat)?;
        }
        Ok(())
    }

    /// Whether a clipboard is available: a live data device on the seat.
    pub fn clipboard(&self) -> bool {
        self.data.as_ref().is_some_and(|d| d.device.is_some())
    }

    /// The seat's selection: the server offer to receive from, kept while
    /// focus lasts, or nothing.
    pub fn selection(&self) -> Option<u32> {
        self.data.as_ref().and_then(|d| d.selection)
    }

    /// The selection's supported text MIME in the spelling it was
    /// announced with, the explicit UTF-8 one preferred; none when the
    /// selection announced neither.
    pub fn selection_mime(&self) -> Option<&str> {
        let data = self.data.as_ref()?;
        data.offers.get(&data.selection?).and_then(Offer::mime)
    }

    /// The live source, whose text the consumer keeps, until `Cancelled`
    /// or the next `offer_selection`.
    pub fn source(&self) -> Option<u32> {
        self.data.as_ref().and_then(|d| d.source)
    }

    /// Asks the selection's offer for its text over `endpoint`, the
    /// producer side of a socket pair the consumer reads from; the
    /// request names the selection's preferred MIME.
    pub fn receive(&mut self, endpoint: &File) -> Result<()> {
        let offer = self.selection().ok_or("no clipboard selection")?;
        let mime = self
            .selection_mime()
            .ok_or("no supported clipboard offer")?
            .to_string();
        let mut body = Builder::new();
        body.string(&mime)?;
        self.connection.send(offer, 1, body, Some(endpoint))
    }

    /// Offers text on the clipboard: a fresh source advertising both text
    /// MIMEs becomes the seat's selection at `serial`, and the previous
    /// source is destroyed. The text is the consumer's, keyed to the
    /// returned id and written when `Send` asks for it.
    pub fn offer_selection(&mut self, serial: u32) -> Result<u32> {
        let (manager, device) = match self.data.as_ref() {
            Some(Data {
                manager,
                device: Some(device),
                ..
            }) => (*manager, *device),
            _ => return Err("no clipboard device".into()),
        };
        let source = self.allocate_kind(Kind::DataSource)?;
        self.connection.words(manager, 0, &[source])?;
        for mime in [UTF8, PLAIN] {
            let mut body = Builder::new();
            body.string(mime)?;
            self.connection.send(source, 0, body, None)?;
        }
        self.connection.words(device, 1, &[source, serial])?;
        let previous = self.data.as_mut().and_then(|d| d.source.replace(source));
        if let Some(previous) = previous {
            self.destroy_source(previous)?;
        }
        Ok(source)
    }

    /// Destroys a source; its id waits for `delete_id`.
    fn destroy_source(&mut self, id: u32) -> Result<()> {
        self.connection.words(id, 1, &[])?;
        self.set_kind(id, Kind::RetiredDataSource)
    }

    /// Destroys a server offer once; the caller queues the barrier its id
    /// is dropped behind, once per batch.
    fn destroy_offer(&mut self, id: u32) -> Result<()> {
        let offer = self
            .data
            .as_mut()
            .and_then(|d| d.offers.get_mut(&id))
            .ok_or("unknown clipboard offer")?;
        if offer.retired {
            return Ok(());
        }
        offer.retired = true;
        self.connection.words(id, 2, &[])
    }

    /// Destroys one offer behind its own barrier.
    fn retire_offer(&mut self, id: u32) -> Result<()> {
        self.destroy_offer(id)?;
        self.queue_barrier()
    }

    /// One outstanding `wl_display.sync` covers every offer retired so
    /// far, by id and generation; later retirements wait for its callback.
    fn queue_barrier(&mut self) -> Result<()> {
        let Some(data) = self.data.as_ref() else {
            return Ok(());
        };
        if !data.barriers.is_empty() {
            return Ok(());
        }
        let retired: Vec<(u32, u64)> = data
            .offers
            .iter()
            .filter(|(_, offer)| offer.retired)
            .map(|(id, offer)| (*id, offer.sequence))
            .collect();
        if retired.is_empty() {
            return Ok(());
        }
        let barrier = self.allocate_kind(Kind::DataSync)?;
        if let Some(data) = self.data.as_mut() {
            data.barriers.insert(barrier, retired);
        }
        self.connection.words(DISPLAY, 0, &[barrier])
    }

    /// The barrier's callback: the offers it covered are dropped, unless
    /// a newer generation reused the id, and the next barrier goes out.
    fn barrier_done(&mut self, id: u32) -> Result<()> {
        let data = self
            .data
            .as_mut()
            .ok_or("clipboard barrier without a device")?;
        let retired = data
            .barriers
            .remove(&id)
            .ok_or("missing clipboard barrier")?;
        for (offer, sequence) in retired {
            if data
                .offers
                .get(&offer)
                .is_some_and(|o| o.retired && o.sequence == sequence)
            {
                data.offers.remove(&offer);
            }
        }
        self.set_kind(id, Kind::RetiredDataSync)?;
        self.queue_barrier()
    }

    /// Focus loss invalidates the selection: every offer is retired.
    fn clear_selection(&mut self) -> Result<()> {
        let Some(data) = self.data.as_mut() else {
            return Ok(());
        };
        data.selection = None;
        let offers: Vec<u32> = data.offers.keys().copied().collect();
        for offer in offers {
            self.destroy_offer(offer)?;
        }
        self.queue_barrier()
    }

    /// Releases the data device and its source; the manager has no
    /// destructor and stays inert, and the offers still drain.
    fn release_data(&mut self) -> Result<()> {
        self.clear_selection()?;
        let source = self.data.as_mut().and_then(|d| d.source.take());
        if let Some(source) = source {
            self.destroy_source(source)?;
        }
        let device = self.data.as_mut().and_then(|d| d.device.take());
        if let Some(device) = device {
            self.connection.words(device, 2, &[])?;
            self.set_kind(device, Kind::RetiredDataDevice)?;
        }
        Ok(())
    }

    fn is_offer(&self, id: u32) -> bool {
        self.data
            .as_ref()
            .is_some_and(|d| d.offers.contains_key(&id))
    }

    /// A server offer's events: its MIME announcements under the budgets;
    /// its actions are validated and ignored.
    fn offer_event(&mut self, message: &Message) -> Result<Handled> {
        let mime = data::offer(message)?;
        let offer = self
            .data
            .as_mut()
            .and_then(|d| d.offers.get_mut(&message.object))
            .ok_or("unknown clipboard offer")?;
        if let Some(mime) = mime {
            offer.announce(mime);
        }
        Ok(Handled::Done)
    }

    /// The data device's events, live or retired: an offer is recorded
    /// under the budgets, and retired at once on a retired device; the
    /// selection retires every other offer; a drag's offer is retired
    /// without accepting or finishing it and is never the selection.
    fn device_event(&mut self, id: u32, message: &Message) -> Result<Handled> {
        let event = data::device(message)?;
        let data = self.data.as_mut().ok_or("data device without a manager")?;
        let active = data.device == Some(id);
        match event {
            DeviceEvent::Offer(offer) => {
                if offer < SERVER_IDS || data.offers.get(&offer).is_some_and(|o| !o.retired) {
                    return Err("invalid server clipboard offer id".into());
                }
                if !data.offers.contains_key(&offer) && data.offers.len() >= OFFER_LIMIT {
                    return Err("clipboard offer budget".into());
                }
                data.sequence = data
                    .sequence
                    .checked_add(1)
                    .ok_or("clipboard offer sequence exhausted")?;
                data.offers.insert(
                    offer,
                    Offer {
                        sequence: data.sequence,
                        retired: false,
                        utf8: None,
                        plain: None,
                        count: 0,
                    },
                );
                if !active {
                    self.retire_offer(offer)?;
                }
                Ok(Handled::Done)
            }
            _ if !active => Ok(Handled::Done),
            DeviceEvent::Selection(offer) => {
                if offer != 0 && !data.offers.get(&offer).is_some_and(|o| !o.retired) {
                    return Err("selection names unknown or retired offer".into());
                }
                data.selection = (offer != 0).then_some(offer);
                let others: Vec<u32> = data
                    .offers
                    .keys()
                    .copied()
                    .filter(|other| Some(*other) != data.selection)
                    .collect();
                for other in others {
                    self.destroy_offer(other)?;
                }
                self.queue_barrier()?;
                Ok(Handled::Clipboard(ClipboardEvent::Selection))
            }
            DeviceEvent::Enter { surface, offer } => {
                if surface != SURFACE {
                    return Err("data-device enter for unknown surface".into());
                }
                if offer != 0 {
                    if data.selection == Some(offer) {
                        return Err("drag reused selection offer".into());
                    }
                    if data.offers.contains_key(&offer) {
                        self.retire_offer(offer)?;
                    }
                }
                Ok(Handled::Done)
            }
            DeviceEvent::Drag => Ok(Handled::Done),
        }
    }

    /// A source's events: a send pops its right and hands it on when the
    /// source is live and the MIME supported, in any ASCII case, or drops
    /// exactly it; cancellation destroys the live source. A retired
    /// source's sends drop their rights.
    fn source_event(&mut self, id: u32, message: &Message) -> Result<Handled> {
        let event = data::source(message)?;
        let active = self.source() == Some(id);
        Ok(match event {
            SourceEvent::Send(mime) => {
                let right = self
                    .connection
                    .pop_descriptor()
                    .ok_or("missing clipboard destination")?;
                let supported = mime.eq_ignore_ascii_case(UTF8) || mime.eq_ignore_ascii_case(PLAIN);
                if active && supported {
                    Handled::Clipboard(ClipboardEvent::Send(right))
                } else {
                    Handled::Done
                }
            }
            SourceEvent::Cancel if active => {
                let source = self.data.as_mut().and_then(|d| d.source.take());
                if let Some(source) = source {
                    self.destroy_source(source)?;
                }
                Handled::Clipboard(ClipboardEvent::Cancelled)
            }
            _ => Handled::Done,
        })
    }

    /// The live keyboard's events are applied to its state; a retired
    /// keyboard's are schema-checked and drained, a keymap's right dropped
    /// unread. `now` retimes an armed repeat when the timing changes.
    fn keyboard_event(&mut self, id: u32, message: &Message, now: u64) -> Result<Handled> {
        let event = keyboard_message(message)?;
        let active = self.keyboard == Some(id);
        Ok(match event {
            KeyboardMessage::Map(format, size) => {
                let fd = self
                    .connection
                    .pop_descriptor()
                    .ok_or("missing keymap descriptor")?;
                if !active {
                    return Ok(Handled::Done);
                }
                self.input.map = None;
                self.input.cancel_repeat();
                self.input.synchronized = false;
                self.held = Held::default();
                let result = read_keymap(fd, format, size).map(|map| self.input.map = Some(map));
                Handled::Keyboard(KeyboardEvent::Keymap(result))
            }
            _ if !active => Handled::Done,
            KeyboardMessage::Enter(surface, keys) => {
                if surface != SURFACE {
                    return Err("keyboard enter for unknown surface".into());
                }
                self.input.focus(&keys, true)?;
                Handled::Keyboard(KeyboardEvent::Focus(true))
            }
            KeyboardMessage::Leave(surface) => {
                if surface != SURFACE {
                    return Err("keyboard leave for unknown surface".into());
                }
                self.input.focus(&[], false)?;
                self.held = Held::default();
                self.clear_selection()?;
                Handled::Keyboard(KeyboardEvent::Focus(false))
            }
            KeyboardMessage::Modifiers(modifiers) => {
                let ready =
                    !self.input.synchronized && self.input.focused && self.input.map.is_some();
                let was = self.input.synchronized && self.input.focused;
                self.input.modifiers(modifiers);
                let held = self.input.map.as_ref().map(|map| map.held(modifiers));
                if ready {
                    self.held = held.unwrap_or_default();
                    Handled::Keyboard(KeyboardEvent::Ready)
                } else if let Some(held) = held.filter(|held| was && *held != self.held) {
                    self.held = held;
                    Handled::Keyboard(KeyboardEvent::Held(held))
                } else {
                    Handled::Done
                }
            }
            KeyboardMessage::Timing(rate, delay) => {
                self.input.timing(rate, delay, now)?;
                Handled::Done
            }
            KeyboardMessage::Key(serial, key, pressed) => match self.input.key(key, pressed) {
                Ok(Some(stroke)) => Handled::Keyboard(KeyboardEvent::Key {
                    serial,
                    key,
                    stroke,
                }),
                Ok(None) => Handled::Done,
                Err(detail) => Handled::Keyboard(KeyboardEvent::Refused(detail)),
            },
        })
    }

    /// The live pointer's events are decoded and handed on; enter and
    /// leave must name this surface, and enter shows the pointer image at
    /// its serial. A retired pointer's events are schema-checked and
    /// drained.
    fn pointer_event(&mut self, id: u32, message: &Message) -> Result<Handled> {
        let event = pointer::decode(message)?;
        if self.pointer != Some(id) {
            return Ok(Handled::Done);
        }
        match event {
            pointer::Event::Enter {
                serial, surface, ..
            } => {
                if surface != SURFACE {
                    return Err("pointer enter for unknown surface".into());
                }
                self.enter = Some(serial);
                self.show_cursor()?;
            }
            pointer::Event::Leave(surface) => {
                if surface != SURFACE {
                    return Err("pointer leave for unknown surface".into());
                }
                self.enter = None;
            }
            _ => {}
        }
        Ok(Handled::Pointer(event))
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
    #[doc(hidden)]
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

    /// Sets the pointer image at the pointer's enter serial, building the
    /// one immutable ARGB pool on the first call. Nothing happens until
    /// the pointer is inside and ARGB was advertised.
    fn show_cursor(&mut self) -> Result<()> {
        let (Some(device), Some(serial), true) = (self.pointer, self.enter, self.argb) else {
            return Ok(());
        };
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
    /// event for an object the table does not know is an error. `now` is
    /// the consumer's clock, the millisecond its `tick` last saw, which
    /// retimes an armed repeat when the keyboard's timing changes.
    pub fn handle(&mut self, message: &Message, now: u64) -> Result<Handled> {
        if self.is_offer(message.object) {
            return self.offer_event(message);
        }
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
                    Kind::Retired
                    | Kind::RetiredBuffer
                    | Kind::RetiredSeat
                    | Kind::RetiredKeyboard
                    | Kind::RetiredPointer
                    | Kind::RetiredDataDevice
                    | Kind::RetiredDataSource
                    | Kind::RetiredDataSync => true,
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
                cursor.finish()?;
                let required = self.required.contains(&id);
                if !required {
                    self.globals.remove(&id);
                    if self
                        .data
                        .as_ref()
                        .is_some_and(|d| d.global == id && d.device.is_some())
                    {
                        self.release_data()?;
                        return Ok(Handled::Clipboard(ClipboardEvent::Released));
                    }
                } else if self.seat.is_some() && self.global_name(id) == Some("wl_seat") {
                    self.release_seat()?;
                    self.forget_global(id);
                    return Ok(Handled::SeatRemoved);
                }
                return Ok(Handled::GlobalRemoved { id, required });
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
                cursor.finish()?;
                self.xrgb |= format == 1;
                if format == 0 && !self.argb {
                    self.argb = true;
                    // A pointer already inside gets its image now.
                    self.show_cursor()?;
                }
                return Ok(Handled::Done);
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
            (id, opcode) if self.seat == Some(id) || self.kind(id)? == Kind::RetiredSeat => {
                match opcode {
                    0 => {
                        let capabilities = cursor.u32()?;
                        cursor.finish()?;
                        if self.seat != Some(id) {
                            return Ok(Handled::Done);
                        }
                        return self.capabilities(id, capabilities);
                    }
                    1 => {
                        if cursor.string()?.len() > NAME_BYTES {
                            return Err("seat name budget".into());
                        }
                        Handled::Done
                    }
                    _ => return Err("unknown seat event".into()),
                }
            }
            (id, _) if matches!(self.kind(id)?, Kind::Keyboard | Kind::RetiredKeyboard) => {
                return self.keyboard_event(id, message, now);
            }
            (id, _) if matches!(self.kind(id)?, Kind::Pointer | Kind::RetiredPointer) => {
                return self.pointer_event(id, message);
            }
            (id, _) if matches!(self.kind(id)?, Kind::DataDevice | Kind::RetiredDataDevice) => {
                return self.device_event(id, message);
            }
            (id, _) if matches!(self.kind(id)?, Kind::DataSource | Kind::RetiredDataSource) => {
                return self.source_event(id, message);
            }
            (id, 0) if self.kind(id)? == Kind::DataSync => {
                cursor.u32()?;
                cursor.finish()?;
                self.barrier_done(id)?;
                return Ok(Handled::Done);
            }
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

enum KeyboardMessage {
    Map(u32, u32),
    Enter(u32, Vec<u32>),
    Leave(u32),
    Key(u32, u32, bool),
    Modifiers(Modifiers),
    Timing(i32, i32),
}

/// The complete `wl_keyboard` v5 through v7 event schema.
fn keyboard_message(message: &Message) -> Result<KeyboardMessage> {
    let mut cursor = Cursor::new(&message.payload);
    let event = match message.opcode {
        0 => KeyboardMessage::Map(cursor.u32()?, cursor.u32()?),
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
            KeyboardMessage::Enter(surface, keys)
        }
        2 => {
            cursor.u32()?;
            KeyboardMessage::Leave(cursor.u32()?)
        }
        3 => {
            let serial = cursor.u32()?;
            cursor.u32()?; // Server timestamps have an unrelated, wrapping epoch.
            let key = cursor.u32()?;
            let state = cursor.u32()?;
            if state > 1 {
                return Err("invalid keyboard state for v5-v7".into());
            }
            KeyboardMessage::Key(serial, key, state == 1)
        }
        4 => {
            cursor.u32()?;
            KeyboardMessage::Modifiers(Modifiers {
                depressed: cursor.u32()?,
                latched: cursor.u32()?,
                locked: cursor.u32()?,
                group: cursor.u32()?,
            })
        }
        5 => KeyboardMessage::Timing(cursor.i32()?, cursor.i32()?),
        _ => return Err("unknown keyboard event".into()),
    };
    cursor.finish()?;
    Ok(event)
}

/// The keymap consumer `UNSAFE.md` §19 records: the one right of a
/// `wl_keyboard.keymap`, read positionally from a regular file covering
/// its advertised extent and compiled whole.
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

/// What `run` drives: a program holding one `Client`.
pub trait App {
    type Tag: Tag;

    fn client(&mut self) -> &mut Client<Self::Tag>;

    /// Whether this event carries a right, for one of the consumer's own
    /// objects, that must have arrived before it is dispatched; the complete
    /// schema is checked here, before waiting. The client answers for its
    /// keymaps itself.
    fn needs_descriptor(&self, message: &Message) -> Result<bool>;

    /// The next event waits for its right. The client cancelled its repeat;
    /// a consumer's own timers that would fire meanwhile are cancelled here.
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
            let wants_right =
                app.client().needs_descriptor(&message)? || app.needs_descriptor(&message)?;
            if wants_right && app.client().descriptors() == 0 {
                if Instant::now() >= deadline {
                    return Err("Wayland descriptor deadline".into());
                }
                // A repeat must not fire ahead of the event it was queued
                // behind.
                app.client().cancel_repeat();
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};

    #[test]
    fn globals_reports_every_advertised_global_in_registry_order() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut client: Client<Never> = Client::new(stream, std::env::temp_dir()).unwrap();
        // Announce out of name order; the accessor yields name order, so an
        // exact-registry check sees the server's announcement sequence.
        for (name, interface, version) in [
            (20u32, "wl_shm", 1u32),
            (10, "wl_compositor", 4),
            (30, "td_portal_manager_v1", 1),
        ] {
            let mut body = Builder::new();
            body.u32(name);
            body.string(interface).unwrap();
            body.u32(version);
            let mut bytes = body.message(REGISTRY, 0).unwrap();
            let message = crate::wire::take(&mut bytes).unwrap().unwrap();
            assert!(matches!(client.handle(&message, 0).unwrap(), Handled::Done));
        }
        let advertised: Vec<(&str, u32)> = client.globals().collect();
        assert_eq!(
            advertised,
            [
                ("wl_compositor", 4),
                ("wl_shm", 1),
                ("td_portal_manager_v1", 1)
            ]
        );
        // Removing an unbound global drops it from the view.
        let mut body = Builder::new();
        body.u32(20);
        let mut bytes = body.message(REGISTRY, 1).unwrap();
        let message = crate::wire::take(&mut bytes).unwrap().unwrap();
        assert!(matches!(
            client.handle(&message, 0).unwrap(),
            Handled::GlobalRemoved {
                required: false,
                ..
            }
        ));
        let advertised: Vec<(&str, u32)> = client.globals().collect();
        assert_eq!(
            advertised,
            [("wl_compositor", 4), ("td_portal_manager_v1", 1)]
        );
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Never {}
    impl Tag for Never {
        fn retired(self) -> bool {
            match self {}
        }
    }

    #[test]
    fn keymap_reads_do_not_move_shared_offsets_and_refuse_bad_sources() {
        let source = include_str!("../tests/fixtures/us.xkb");
        let mut file = backing_file(&std::env::temp_dir(), source.len() + 1).unwrap();
        file.write_all_at(source.as_bytes(), 0).unwrap();
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
}
