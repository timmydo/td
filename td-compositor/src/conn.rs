//! The Wayland client transport, and the object-id map its users share.
//!
//! Extracted from the demo client so the terminal is a second USER of one
//! connection rather than a second copy of it. Nothing here knows what is
//! being drawn: it connects, allocates and recycles object ids, frames
//! messages, carries descriptors, and answers the three events every client
//! must answer identically — the display's protocol error, its `delete_id`,
//! and the shell's ping.

use crate::render::BYTES_PER_PIXEL;
use crate::{sys, wire};
use std::collections::{BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

pub const DISPLAY: u32 = 1;
pub const REGISTRY: u32 = 2;
pub const SYNC_CALLBACK: u32 = 3;
pub const COMPOSITOR: u32 = 4;
pub const SHM: u32 = 5;
pub const XDG_WM_BASE: u32 = 6;
pub const SURFACE: u32 = 7;
pub const XDG_SURFACE: u32 = 8;
pub const XDG_TOPLEVEL: u32 = 9;
pub const SEAT: u32 = 10;
pub const KEYBOARD: u32 = 11;
pub const POINTER: u32 = 12;
// Where a client's dynamic ids begin is a property of the CLIENT, not of the
// transport: it is one past the last fixed id that client actually creates, so
// a client binding fewer globals has a lower one. Wayland requires client ids
// to be allocated DENSELY — libwayland's object map refuses an insert past the
// end of its array — so a client that skipped the ids it never created could be
// disconnected by a compliant compositor. td's own server checks only
// uniqueness, which is why this cannot be left to the server to catch.

pub const CONNECT_ATTEMPTS: usize = 300;
pub const MAX_PENDING_FDS: usize = 8;
pub const RECEIVE_BUFFER_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy)]
pub struct Global {
    pub name: u32,
    pub version: u32,
}

/// The fields are private because `record` is the rule — highest advertised
/// version wins — and a direct assignment from any module would be a way
/// around it. They were module-private before the extraction; these keep them
/// that way.
#[derive(Default)]
pub struct Globals {
    compositor: Option<Global>,
    shm: Option<Global>,
    xdg_wm_base: Option<Global>,
    seat: Option<Global>,
}

impl Globals {
    pub fn compositor(&self) -> Option<Global> {
        self.compositor
    }

    pub fn shm(&self) -> Option<Global> {
        self.shm
    }

    pub fn xdg_wm_base(&self) -> Option<Global> {
        self.xdg_wm_base
    }

    pub fn seat(&self) -> Option<Global> {
        self.seat
    }

    pub fn record(&mut self, name: u32, interface: &str, version: u32) {
        let global = Global { name, version };
        match interface {
            "wl_compositor"
                if self
                    .compositor
                    .is_none_or(|current| version > current.version) =>
            {
                self.compositor = Some(global)
            }
            "wl_shm" if self.shm.is_none_or(|current| version > current.version) => {
                self.shm = Some(global)
            }
            "xdg_wm_base"
                if self
                    .xdg_wm_base
                    .is_none_or(|current| version > current.version) =>
            {
                self.xdg_wm_base = Some(global)
            }
            "wl_seat" if self.seat.is_none_or(|current| version > current.version) => {
                self.seat = Some(global)
            }
            _ => {}
        }
    }

    pub fn require(
        global: Option<Global>,
        interface: &str,
        minimum: u32,
        maximum: u32,
    ) -> Result<(u32, u32), String> {
        let global = global.ok_or_else(|| format!("compositor did not advertise {interface}"))?;
        if global.version < minimum {
            return Err(format!(
                "{interface} version {} is below required version {minimum}",
                global.version
            ));
        }
        Ok((global.name, global.version.min(maximum)))
    }
}

pub struct Connection {
    stream: UnixStream,
    buffered: Vec<u8>,
    deadline: Option<Instant>,
    first_dynamic_id: u32,
    next_id: u32,
    free_ids: BTreeSet<u32>,
    pending_fds: VecDeque<RawFd>,
    incoming: [u8; RECEIVE_BUFFER_BYTES],
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.discard_pending_fds();
    }
}

impl Connection {
    pub fn connect(
        path: &Path,
        deadline: Instant,
        first_dynamic_id: u32,
    ) -> Result<Connection, String> {
        let mut last = None;
        for attempt in 0..CONNECT_ATTEMPTS {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|duration| !duration.is_zero())
                .ok_or_else(|| "Wayland presentation handshake timed out".to_string())?;
            match UnixStream::connect(path) {
                Ok(stream) => {
                    return Ok(Connection::over(stream, Some(deadline), first_dynamic_id))
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    last = Some(error);
                    if attempt + 1 < CONNECT_ATTEMPTS {
                        thread::sleep(remaining.min(Duration::from_millis(100)));
                    }
                }
                Err(error) => {
                    return Err(format!(
                        "connect Wayland socket {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        Err(format!(
            "connect Wayland socket {} after {CONNECT_ATTEMPTS} attempts: {}",
            path.display(),
            last.map_or_else(|| "unknown error".to_string(), |error| error.to_string())
        ))
    }

    /// A connection over an already-open stream. THE constructor: `connect`
    /// dials and hands the stream here, so a field added to `Connection` is
    /// initialised in one place rather than once per caller. `deadline` is
    /// what separates a handshake (bounded) from a running client (not).
    pub fn over(
        stream: UnixStream,
        deadline: Option<Instant>,
        first_dynamic_id: u32,
    ) -> Connection {
        Connection {
            stream,
            buffered: Vec::with_capacity(16 * 1024),
            deadline,
            first_dynamic_id,
            next_id: first_dynamic_id,
            free_ids: BTreeSet::new(),
            pending_fds: VecDeque::new(),
            incoming: [0; RECEIVE_BUFFER_BYTES],
        }
    }

    pub fn remaining(&self) -> Result<Option<Duration>, String> {
        self.deadline
            .map(|deadline| {
                deadline
                    .checked_duration_since(Instant::now())
                    .filter(|duration| !duration.is_zero())
                    .ok_or_else(|| "Wayland presentation handshake timed out".to_string())
            })
            .transpose()
    }

    /// Bound how long an event wait may block. The handshake sets this from
    /// its deadline; a test sets it so a silent peer fails rather than hangs.
    #[cfg(test)]
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), String> {
        self.stream
            .set_read_timeout(timeout)
            .map_err(|e| format!("set Wayland event timeout: {e}"))
    }

    pub fn finish_handshake(&mut self) -> Result<(), String> {
        self.stream
            .set_read_timeout(None)
            .map_err(|e| format!("clear Wayland handshake timeout: {e}"))?;
        self.deadline = None;
        Ok(())
    }

    pub fn send(&mut self, object: u32, opcode: u16, builder: wire::Builder) -> Result<(), String> {
        let bytes = builder.message(object, opcode)?;
        self.stream
            .write_all(&bytes)
            .map_err(|e| format!("write Wayland request object={object} opcode={opcode}: {e}"))
    }

    pub fn send_with_fd(
        &mut self,
        object: u32,
        opcode: u16,
        builder: wire::Builder,
        file: &File,
    ) -> Result<(), String> {
        let bytes = builder.message(object, opcode)?;
        sys::send_with_fd(&self.stream, &bytes, file.as_raw_fd())
            .map_err(|e| format!("send Wayland request descriptor: {e}"))
    }

    pub fn next(&mut self) -> Result<wire::Message, String> {
        loop {
            let remaining = self.remaining()?;
            if let Some(message) = wire::take(&mut self.buffered)? {
                return Ok(message);
            }
            let wanted = match wire::header(&self.buffered)? {
                Some((_, _, size)) => size.saturating_sub(self.buffered.len()),
                None => wire::HEADER_SIZE.saturating_sub(self.buffered.len()),
            };
            if wanted == 0 {
                return Err("Wayland event parser made no progress".into());
            }
            let capacity = wanted.min(self.incoming.len());
            let input = self
                .incoming
                .get_mut(..capacity)
                .ok_or_else(|| "Wayland receive bound escaped input buffer".to_string())?;
            if let Some(remaining) = remaining {
                self.stream
                    .set_read_timeout(Some(remaining))
                    .map_err(|e| format!("set Wayland handshake timeout: {e}"))?;
            }
            let received = match sys::recv_with_fds(&self.stream, input) {
                Ok(received) => received,
                Err(sys::ReceiveError::Disconnected) => {
                    return Err("Wayland compositor closed the connection".into())
                }
                Err(sys::ReceiveError::TimedOut) if self.deadline.is_some() => {
                    return Err("Wayland presentation handshake timed out".into())
                }
                Err(sys::ReceiveError::TimedOut) => {
                    return Err("Wayland event wait timed out".into())
                }
                Err(sys::ReceiveError::Failure(error)) => {
                    return Err(format!("receive Wayland event: {error}"))
                }
            };
            if received.count == 0 {
                sys::discard_received(&received.fds);
                return Err("Wayland compositor closed the connection".into());
            }
            if self.pending_fds.len().saturating_add(received.fds.len()) > MAX_PENDING_FDS {
                sys::discard_received(&received.fds);
                self.discard_pending_fds();
                return Err(format!(
                    "Wayland client queued more than {MAX_PENDING_FDS} descriptors"
                ));
            }
            let bytes = input
                .get(..received.count)
                .ok_or_else(|| "Wayland read count escaped input buffer".to_string())?;
            self.buffered.extend_from_slice(bytes);
            self.pending_fds.extend(received.fds);
        }
    }

    pub fn handle_common(&mut self, message: &wire::Message) -> Result<bool, String> {
        if message.object == DISPLAY && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            let object = args.u32()?;
            let code = args.u32()?;
            let text = args.string()?;
            args.finish()?;
            return Err(format!(
                "Wayland protocol error on object {object}, code {code}: {text}"
            ));
        }
        if message.object == XDG_WM_BASE && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            let serial = args.u32()?;
            args.finish()?;
            let mut pong = wire::Builder::new();
            pong.u32(serial);
            self.send(XDG_WM_BASE, 3, pong)?;
            return Ok(true);
        }
        if message.object == DISPLAY && message.opcode == 1 {
            let mut args = wire::Cursor::new(&message.payload);
            let id = args.u32()?;
            args.finish()?;
            if id >= self.first_dynamic_id {
                if id >= self.next_id {
                    return Err(format!("compositor deleted unallocated object {id}"));
                }
                if !self.free_ids.insert(id) {
                    return Err(format!("compositor deleted object {id} twice"));
                }
            }
            return Ok(true);
        }
        Ok(false)
    }

    pub fn allocate_id(&mut self) -> Result<u32, String> {
        if let Some(id) = self.free_ids.pop_first() {
            return Ok(id);
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .filter(|next| *next != 0)
            .ok_or_else(|| "Wayland object id space exhausted".to_string())?;
        Ok(id)
    }

    pub fn take_fd(&mut self, purpose: &str) -> Result<File, String> {
        let fd = self
            .pending_fds
            .pop_front()
            .ok_or_else(|| format!("{purpose} event arrived without a descriptor"))?;
        sys::duplicate_received(fd)
    }

    /// The next id this connection would hand out. A test reads it to show
    /// that a path allocated nothing, and sets it to make the numbers in an
    /// assertion legible.
    #[cfg(test)]
    pub fn next_id_for_test(&self) -> u32 {
        self.next_id
    }

    #[cfg(test)]
    pub fn set_next_id_for_test(&mut self, id: u32) {
        self.next_id = id;
    }

    /// Hand the connection a descriptor as though an event had carried one,
    /// so the discarding paths can be driven without a compositor.
    #[cfg(test)]
    pub fn queue_fd_for_test(&mut self, fd: RawFd) {
        self.pending_fds.push_back(fd);
    }

    /// How many descriptors arrived that nobody claimed. A client that
    /// finishes its handshake holding one has misread an event.
    pub fn pending_fd_count(&self) -> usize {
        self.pending_fds.len()
    }

    pub fn discard_pending_fds(&mut self) {
        while let Some(fd) = self.pending_fds.pop_front() {
            sys::discard_received(&[fd]);
        }
    }
}

/// Bind an advertised global to a fixed object id. Both clients assign the
/// same ids to the same interfaces, so this is transport rather than policy.
pub fn bind(
    connection: &mut Connection,
    name: u32,
    interface: &str,
    version: u32,
    id: u32,
) -> Result<(), String> {
    let mut request = wire::Builder::new();
    request.u32(name);
    request.string(interface)?;
    request.u32(version);
    request.u32(id);
    connection.send(REGISTRY, 0, request)
}

/// The registry round-trip every client makes: ask for the registry, ask for
/// a sync behind it, and record what is advertised until the sync answers.
/// The sync is what makes the list COMPLETE rather than merely long.
pub fn discover_globals(connection: &mut Connection) -> Result<Globals, String> {
    let mut registry = wire::Builder::new();
    registry.u32(REGISTRY);
    connection.send(DISPLAY, 1, registry)?;

    let mut sync = wire::Builder::new();
    sync.u32(SYNC_CALLBACK);
    connection.send(DISPLAY, 0, sync)?;

    let mut globals = Globals::default();
    loop {
        let message = connection.next()?;
        if connection.handle_common(&message)? {
            continue;
        }
        if message.object == REGISTRY && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            let name = args.u32()?;
            let interface = args.string()?;
            let version = args.u32()?;
            args.finish()?;
            globals.record(name, &interface, version);
        } else if message.object == SYNC_CALLBACK && message.opcode == 0 {
            let mut args = wire::Cursor::new(&message.payload);
            args.u32()?;
            args.finish()?;
            return Ok(globals);
        }
    }
}

/// Create the surface, its xdg role and its toplevel, name it, and commit
/// with nothing attached — which is what asks for the first configure. Pure
/// transport differing between clients only by the title, so both use it.
pub fn create_surface(connection: &mut Connection, title: &str) -> Result<(), String> {
    let mut surface = wire::Builder::new();
    surface.u32(SURFACE);
    connection.send(COMPOSITOR, 0, surface)?;

    let mut xdg_surface = wire::Builder::new();
    xdg_surface.u32(XDG_SURFACE);
    xdg_surface.u32(SURFACE);
    connection.send(XDG_WM_BASE, 2, xdg_surface)?;

    let mut toplevel = wire::Builder::new();
    toplevel.u32(XDG_TOPLEVEL);
    connection.send(XDG_SURFACE, 1, toplevel)?;

    let mut named = wire::Builder::new();
    named.string(title)?;
    connection.send(XDG_TOPLEVEL, 2, named)?;
    connection.send(SURFACE, 6, wire::Builder::new())
}

/// An anonymous wl_shm backing file holding `pixels`: created 0600 under
/// `directory`, written, and UNLINKED before it is handed over, so the only
/// reference left is the descriptor the compositor receives. `stem` names the
/// transient path, which exists only long enough to be opened.
pub fn backing_file(directory: &Path, stem: &str, pixels: &[u8]) -> Result<File, String> {
    let pid = std::process::id();
    for attempt in 0..64u32 {
        let path = directory.join(format!("{stem}-{pid}-{attempt}.shm"));
        let mut file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "create wl_shm backing file {}: {error}",
                    path.display()
                ));
            }
        };
        let write = file
            .write_all(pixels)
            .map_err(|e| format!("write wl_shm backing file {}: {e}", path.display()));
        let remove = fs::remove_file(&path)
            .map_err(|e| format!("unlink wl_shm file {}: {e}", path.display()));
        // Unlink before propagating a write failure so every created path is
        // cleaned up.
        write?;
        remove?;
        return Ok(file);
    }
    Err(format!(
        "could not create a unique wl_shm file in {}",
        directory.display()
    ))
}

/// Put one frame on the surface: a pool over `pixels`, a buffer covering it,
/// attach, damage, a frame callback, commit. Returns the buffer and callback
/// ids, which are what the caller waits on — the compositor owns the buffer
/// until it releases it, and the frame is not on screen until the callback
/// fires. The pool is destroyed immediately; the buffer keeps the mapping.
pub fn attach_frame(
    connection: &mut Connection,
    directory: &Path,
    stem: &str,
    pixels: &[u8],
    width: usize,
    height: usize,
) -> Result<(u32, u32), String> {
    let file = backing_file(directory, stem, pixels)?;
    let pool_id = connection.allocate_id()?;
    let buffer_id = connection.allocate_id()?;
    let callback_id = connection.allocate_id()?;
    let bytes = i32::try_from(pixels.len()).map_err(|_| "wl_shm pool exceeds i32".to_string())?;
    let pixel_width = i32::try_from(width).map_err(|_| "surface width exceeds i32".to_string())?;
    let pixel_height =
        i32::try_from(height).map_err(|_| "surface height exceeds i32".to_string())?;
    let stride = width
        .checked_mul(BYTES_PER_PIXEL)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| "surface stride exceeds i32".to_string())?;

    let mut pool = wire::Builder::new();
    pool.u32(pool_id);
    pool.i32(bytes);
    connection.send_with_fd(SHM, 0, pool, &file)?;

    let mut buffer = wire::Builder::new();
    buffer.u32(buffer_id);
    buffer.i32(0);
    buffer.i32(pixel_width);
    buffer.i32(pixel_height);
    buffer.i32(stride);
    buffer.u32(crate::scene::SHM_XRGB8888);
    connection.send(pool_id, 0, buffer)?;
    connection.send(pool_id, 1, wire::Builder::new())?;

    let mut attach = wire::Builder::new();
    attach.u32(buffer_id);
    attach.i32(0);
    attach.i32(0);
    connection.send(SURFACE, 1, attach)?;

    let mut damage = wire::Builder::new();
    damage.i32(0);
    damage.i32(0);
    damage.i32(pixel_width);
    damage.i32(pixel_height);
    connection.send(SURFACE, 9, damage)?;

    let mut frame = wire::Builder::new();
    frame.u32(callback_id);
    connection.send(SURFACE, 3, frame)?;
    connection.send(SURFACE, 6, wire::Builder::new())?;
    Ok((buffer_id, callback_id))
}
