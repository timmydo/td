//! The Wayland client transport, and the object-id map its users share.
//!
//! Extracted from the demo client so the terminal is a second USER of one
//! connection rather than a second copy of it. Nothing here knows what is
//! being drawn: it connects, allocates and recycles object ids, frames
//! messages, carries descriptors, and answers the three events every client
//! must answer identically — the display's protocol error, its `delete_id`,
//! and the shell's ping.

use crate::keyboard::XKB_KEYMAP;
use crate::render::BYTES_PER_PIXEL;
use crate::{sys, wire};
use std::collections::{BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::FileExt;
use std::os::unix::fs::OpenOptionsExt;
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
const CLIPBOARD_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const CLIPBOARD_WRITE_RETRY: Duration = Duration::from_millis(1);

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
    data_device_manager: Option<Global>,
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

    pub fn data_device_manager(&self) -> Option<Global> {
        self.data_device_manager
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
            "wl_data_device_manager"
                if self
                    .data_device_manager
                    .is_none_or(|current| version > current.version) =>
            {
                self.data_device_manager = Some(global)
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
    /// Whether the reading half has moved to a thread. Reading from both
    /// would split a message between them.
    reader_detached: bool,
}

/// The reading half of a connection, once it has been detached. Owns a second
/// handle on the same socket, the bytes already read past the last message,
/// and any descriptors that arrived unclaimed — so nothing a `Connection` had
/// buffered is lost by moving reads to a thread.
pub struct Reader {
    stream: UnixStream,
    buffered: Vec<u8>,
    pending_fds: VecDeque<RawFd>,
    incoming: [u8; RECEIVE_BUFFER_BYTES],
}

impl Drop for Reader {
    fn drop(&mut self) {
        discard_fds(&mut self.pending_fds);
    }
}

impl Reader {
    pub fn next(&mut self) -> Result<wire::Message, String> {
        // No deadline, and that is not an omission: detaching requires the
        // handshake to have finished, which is the only thing a deadline
        // bounds. A running client waits as long as the compositor is quiet.
        read_next(
            &self.stream,
            &mut self.buffered,
            &mut self.pending_fds,
            &mut self.incoming,
            None,
        )
    }

    /// How many descriptors arrived that nobody claimed. The terminal takes
    /// its keymap descriptor and no other, so a leftover is a compositor
    /// sending one nothing asked for.
    pub fn pending_fd_count(&self) -> usize {
        self.pending_fds.len()
    }

    /// Claim the exact descriptor carried by the event just read. Unlike the
    /// handshake keymap path this does not reopen through `/proc`: a selection
    /// endpoint may be a pipe or socket, and its open-file description is the
    /// capability the destination supplied.
    pub fn take_file(&mut self, purpose: &str) -> Result<File, String> {
        let fd = self
            .pending_fds
            .pop_front()
            .ok_or_else(|| format!("{purpose} event arrived without a descriptor"))?;
        let endpoint = sys::ReceivedFd::adopt(fd)?;
        Ok(sys::ReceivedFd::into_file(endpoint))
    }
}

/// Write one source payload without letting an adversarial destination park
/// td-term's sole writer forever. `O_NONBLOCK` is restored even on failure
/// because the receiver may have retained a duplicate of the endpoint.
pub fn write_clipboard(file: &mut File, bytes: &[u8]) -> Result<(), String> {
    write_clipboard_with_timeout(file, bytes, CLIPBOARD_WRITE_TIMEOUT)
}

fn write_clipboard_with_timeout(
    file: &mut File,
    bytes: &[u8],
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "clipboard write deadline overflowed".to_string())?;
    let flags = sys::make_nonblocking(file)?;
    let mut offset = 0usize;
    let result = loop {
        let Some(remaining) = bytes.get(offset..) else {
            break Err("clipboard write offset escaped its payload".into());
        };
        if remaining.is_empty() {
            break Ok(());
        }
        if Instant::now() >= deadline {
            break Err(format!(
                "clipboard destination exceeded {} milliseconds",
                timeout.as_millis()
            ));
        }
        match file.write(remaining) {
            Ok(0) => break Err("clipboard destination accepted zero bytes".into()),
            Ok(written) if written <= remaining.len() => offset += written,
            Ok(written) => {
                break Err(format!(
                    "clipboard destination accepted invalid byte count {written}"
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    break Err(format!(
                        "clipboard destination exceeded {} milliseconds",
                        timeout.as_millis()
                    ));
                }
                thread::sleep(CLIPBOARD_WRITE_RETRY);
            }
            Err(error) => break Err(format!("write clipboard selection: {error}")),
        }
    };
    let restored = sys::restore_status_flags(file, flags);
    match (result, restored) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(write), Err(restore)) => Err(format!("{write}; {restore}")),
    }
}

/// The whole wl_keyboard keymap check: format, announced size, the file's own
/// size, and its bytes against td's pinned keymap plus its NUL.
///
/// It lives here rather than in a client because BOTH clients need it — the
/// demo and the terminal — and a validation copied is a validation that stays
/// correct until exactly one copy is fixed.
pub fn verify_keymap(file: &File, format: u32, size: u32) -> Result<(), String> {
    if format != 1 {
        return Err(format!("unsupported wl_keyboard keymap format {format}"));
    }
    let expected_size = XKB_KEYMAP
        .len()
        .checked_add(1)
        .ok_or_else(|| "expected XKB keymap size overflow".to_string())?;
    let announced_size =
        usize::try_from(size).map_err(|_| "wl_keyboard keymap size escaped usize".to_string())?;
    if announced_size != expected_size {
        return Err(format!(
            "wl_keyboard keymap has size {announced_size}, expected {expected_size}"
        ));
    }
    let metadata_size = usize::try_from(
        file.metadata()
            .map_err(|e| format!("stat wl_keyboard keymap: {e}"))?
            .len(),
    )
    .map_err(|_| "wl_keyboard keymap file size escaped usize".to_string())?;
    if metadata_size != expected_size {
        return Err(format!(
            "wl_keyboard keymap file has size {metadata_size}, expected {expected_size}"
        ));
    }
    let bytes = read_keymap_bytes(file, expected_size)?;
    let body = bytes
        .get(..XKB_KEYMAP.len())
        .ok_or_else(|| "wl_keyboard keymap is truncated".to_string())?;
    if body != XKB_KEYMAP.as_bytes() || bytes.last().copied() != Some(0) {
        return Err("wl_keyboard keymap differs from td's pinned keymap".into());
    }
    Ok(())
}

/// POSITIONED reads, which is §11's rule and not a style choice. The
/// compositor sends every client a duplicate of ONE open file description, so
/// on the wire the read offset is SHARED, and a sequential read would leave it
/// at end-of-file for whoever binds a keyboard next.
///
/// That does not bite today, and the reason is worth naming so nobody removes
/// this thinking it does nothing: `Connection::take_fd` REOPENS the received
/// descriptor through `/proc/self/fd/N`, which is a new description with its
/// own offset. That reopen exists for descriptor safety; the separate exact
/// clipboard-endpoint adoption cannot replace it. A change here would
/// therefore be reviewed for descriptor ownership and not necessarily for what
/// it does to a file offset. This is the guard §11 names, and the two are
/// independent.
///
/// The size is already pinned by the caller's metadata check, so this asks
/// for exactly that many bytes and treats a short answer as the file being
/// something other than what was announced.
fn read_keymap_bytes(file: &File, expected_size: usize) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0u8; expected_size];
    let mut filled = 0usize;
    while filled < expected_size {
        let offset =
            u64::try_from(filled).map_err(|_| "keymap read offset escaped u64".to_string())?;
        let rest = bytes
            .get_mut(filled..)
            .ok_or_else(|| "keymap read bound escaped its buffer".to_string())?;
        match file.read_at(rest, offset) {
            Ok(0) => {
                return Err(format!(
                    "wl_keyboard keymap read {filled} bytes, expected {expected_size}"
                ))
            }
            Ok(count) => filled = filled.saturating_add(count),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("read wl_keyboard keymap: {error}")),
        }
    }
    // A file that GREW between the caller's `stat` and here is not the file
    // whose size was announced, and the announced size is what was validated.
    // One byte past the end, positioned like the rest, is what says so.
    let mut past = [0u8; 1];
    let end =
        u64::try_from(expected_size).map_err(|_| "keymap end offset escaped u64".to_string())?;
    loop {
        match file.read_at(&mut past, end) {
            Ok(0) => break,
            Ok(_) => {
                return Err(format!(
                    "wl_keyboard keymap read {} bytes, expected {expected_size}",
                    expected_size.saturating_add(1)
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("read wl_keyboard keymap: {error}")),
        }
    }
    Ok(bytes)
}

/// Send an event carrying a descriptor, for tests that need a client to
/// RECEIVE one. It lives here because this is a module the descriptor roster
/// already names: `term_client.rs` reaches no confined syscall, and a test
/// there spelling one would be a roster change rather than a test.
#[cfg(test)]
pub fn send_event_with_fd(peer: &UnixStream, bytes: &[u8], file: &File) -> std::io::Result<()> {
    sys::send_with_fd(peer, bytes, file.as_raw_fd())
}

fn discard_fds(fds: &mut VecDeque<RawFd>) {
    while let Some(fd) = fds.pop_front() {
        sys::discard_received(&[fd]);
    }
}

/// One event, from whichever half is doing the reading. A free function over
/// the pieces rather than a method, so `Connection` and `Reader` cannot drift
/// into two dialects of the same parser.
fn read_next(
    stream: &UnixStream,
    buffered: &mut Vec<u8>,
    pending_fds: &mut VecDeque<RawFd>,
    incoming: &mut [u8],
    deadline: Option<Instant>,
) -> Result<wire::Message, String> {
    loop {
        let remaining = deadline
            .map(|deadline| {
                deadline
                    .checked_duration_since(Instant::now())
                    .filter(|duration| !duration.is_zero())
                    .ok_or_else(|| "Wayland presentation handshake timed out".to_string())
            })
            .transpose()?;
        if let Some(message) = wire::take(buffered)? {
            return Ok(message);
        }
        let wanted = match wire::header(buffered)? {
            Some((_, _, size)) => size.saturating_sub(buffered.len()),
            None => wire::HEADER_SIZE.saturating_sub(buffered.len()),
        };
        if wanted == 0 {
            return Err("Wayland event parser made no progress".into());
        }
        let capacity = wanted.min(incoming.len());
        let input = incoming
            .get_mut(..capacity)
            .ok_or_else(|| "Wayland receive bound escaped input buffer".to_string())?;
        if let Some(remaining) = remaining {
            stream
                .set_read_timeout(Some(remaining))
                .map_err(|e| format!("set Wayland handshake timeout: {e}"))?;
        }
        let received = match sys::recv_with_fds(stream, input) {
            Ok(received) => received,
            Err(sys::ReceiveError::Disconnected) => {
                return Err("Wayland compositor closed the connection".into())
            }
            Err(sys::ReceiveError::TimedOut) if deadline.is_some() => {
                return Err("Wayland presentation handshake timed out".into())
            }
            Err(sys::ReceiveError::TimedOut) => return Err("Wayland event wait timed out".into()),
            Err(sys::ReceiveError::Failure(error)) => {
                return Err(format!("receive Wayland event: {error}"))
            }
        };
        if received.count == 0 {
            sys::discard_received(&received.fds);
            return Err("Wayland compositor closed the connection".into());
        }
        if pending_fds.len().saturating_add(received.fds.len()) > MAX_PENDING_FDS {
            sys::discard_received(&received.fds);
            discard_fds(pending_fds);
            return Err(format!(
                "Wayland client queued more than {MAX_PENDING_FDS} descriptors"
            ));
        }
        let bytes = input
            .get(..received.count)
            .ok_or_else(|| "Wayland read count escaped input buffer".to_string())?;
        buffered.extend_from_slice(bytes);
        pending_fds.extend(received.fds);
    }
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
            reader_detached: false,
        }
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
        if self.reader_detached {
            return Err("Wayland events are being read on another thread".into());
        }
        read_next(
            &self.stream,
            &mut self.buffered,
            &mut self.pending_fds,
            &mut self.incoming,
            self.deadline,
        )
    }

    /// Hand the reading half to another thread, and stop reading here.
    ///
    /// Wayland reads BLOCK, and a client whose main loop has a second source
    /// to serve — a terminal with a child on a PTY — cannot afford to block
    /// in one of them. Every WRITE stays with the connection, so request
    /// order remains the property of one thread rather than something two
    /// have to agree about; what moves is the socket handle for reading, the
    /// bytes already buffered, and any descriptors already received.
    ///
    /// Detaching is one-way, which is what makes "who reads" answerable by
    /// looking rather than by reasoning: this connection refuses to read
    /// afterwards, so a second reader cannot appear by accident and take
    /// half of a message the first is waiting for.
    pub fn detach_reader(&mut self) -> Result<Reader, String> {
        if self.reader_detached {
            return Err("the Wayland reading half was already detached".into());
        }
        // Only after the handshake, and that is a real constraint rather than
        // a convention. `read_next` SETS the socket's read timeout from a
        // live deadline and never clears it, so a connection whose deadline
        // went away while a timeout stayed behind would give the reader a
        // thread that wakes with `TimedOut` on a compositor that is merely
        // quiet. `finish_handshake` clears both together and is the only way
        // to clear either, so requiring it here is what makes that pairing a
        // property instead of an ordering somebody has to remember.
        if self.deadline.is_some() {
            return Err("the Wayland reading half detached before the handshake finished".into());
        }
        let stream = self
            .stream
            .try_clone()
            .map_err(|e| format!("clone the Wayland connection for reading: {e}"))?;
        self.reader_detached = true;
        Ok(Reader {
            stream,
            buffered: std::mem::take(&mut self.buffered),
            pending_fds: std::mem::take(&mut self.pending_fds),
            incoming: [0; RECEIVE_BUFFER_BYTES],
        })
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
        discard_fds(&mut self.pending_fds);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};
    use std::os::fd::{IntoRawFd, OwnedFd};
    use std::time::Duration;

    /// The read leaves the offset exactly where it found it, which is §11's
    /// rule stated as the property rather than as a mechanism. Reading twice
    /// is not enough on its own: an implementation that SEEKS to zero and then
    /// reads sequentially answers twice and still leaves the description at
    /// end-of-file for whoever holds it next.
    ///
    /// Nothing in td depends on this today — `sys::duplicate_received` reopens
    /// a received descriptor through `/proc/self/fd/N`, so every client gets a
    /// private offset and a sequential read would work anyway. That reopen is
    /// the guard that is load-bearing; this is the one §11 names.
    #[test]
    fn reading_the_keymap_leaves_the_offset_alone() {
        let expected = {
            let mut bytes = XKB_KEYMAP.as_bytes().to_vec();
            bytes.push(0);
            bytes
        };
        let file = backing_file(&std::env::temp_dir(), "td-ui-demo-test", &expected).unwrap();
        let file = sys::duplicate_received(file.into_raw_fd()).unwrap();
        // Somewhere that is neither the start nor the end, so neither a
        // rewind nor a read-to-end can leave it looking untouched.
        let parked = 3;
        (&file).seek(SeekFrom::Start(parked)).unwrap();
        for read in 0..2 {
            let bytes = read_keymap_bytes(&file, expected.len())
                .unwrap_or_else(|error| panic!("read {read} failed: {error}"));
            assert_eq!(bytes, expected);
            assert_eq!(
                (&file).stream_position().unwrap(),
                parked,
                "read {read} moved the description's offset"
            );
        }
    }

    /// A file that SHRANK between the caller's `stat` and the read. The
    /// helper's contract is that a short answer is the file being something
    /// other than what was announced, and without this the arm that says so
    /// can be a `break` returning a zero-padded buffer.
    #[test]
    fn a_keymap_shorter_than_announced_is_refused() {
        let bytes = vec![7u8; 8];
        let file = backing_file(&std::env::temp_dir(), "td-ui-demo-test", &bytes).unwrap();
        let file = sys::duplicate_received(file.into_raw_fd()).unwrap();
        let error = read_keymap_bytes(&file, 16).unwrap_err();
        assert!(error.contains("read 8 bytes, expected 16"), "{error}");
    }

    #[test]
    fn keymap_read_is_bounded_against_growth_after_metadata() {
        let bytes = vec![7u8; 17];
        let file = backing_file(&std::env::temp_dir(), "td-ui-demo-test", &bytes).unwrap();
        let file = sys::duplicate_received(file.into_raw_fd()).unwrap();
        let error = read_keymap_bytes(&file, 16).unwrap_err();
        assert!(error.contains("read 17 bytes, expected 16"));
    }

    fn event(object: u32, opcode: u16, payload: &[u8]) -> Vec<u8> {
        let mut builder = wire::Builder::new();
        for word in payload.chunks(4) {
            let mut bytes = [0u8; 4];
            for (target, source) in bytes.iter_mut().zip(word) {
                *target = *source;
            }
            builder.u32(u32::from_ne_bytes(bytes));
        }
        builder.message(object, opcode).unwrap()
    }

    fn pair() -> (Connection, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        (Connection::over(ours, None, 10), theirs)
    }

    #[test]
    fn a_full_clipboard_endpoint_times_out_and_restores_blocking_status() {
        let (mut endpoint, _receiver) = UnixStream::pair().unwrap();
        endpoint.set_nonblocking(true).unwrap();
        let chunk = [0u8; 4096];
        let mut full = false;
        for _ in 0..1024 {
            match endpoint.write(&chunk) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    full = true;
                    break;
                }
                Err(error) => panic!("fill clipboard endpoint: {error}"),
            }
        }
        assert!(full, "socket capacity escaped the bounded fixture");
        endpoint.set_nonblocking(false).unwrap();
        let mut endpoint = File::from(OwnedFd::from(endpoint));

        let error = write_clipboard_with_timeout(&mut endpoint, b"x", Duration::from_millis(5))
            .unwrap_err();
        assert!(error.contains("exceeded 5 milliseconds"), "{error}");
        let flags = sys::make_nonblocking(&endpoint).unwrap();
        assert_eq!(flags & 0o4000, 0, "blocking status was not restored");
        sys::restore_status_flags(&endpoint, flags).unwrap();
    }

    /// Detaching moves the bytes already READ but not yet parsed, and what
    /// puts bytes there is a SHORT read: `read_next` asks for exactly what
    /// the message in hand still needs, so it never over-reads past one, but
    /// a receive that returns less leaves a fragment behind. A detach that
    /// handed over a bare descriptor would drop that fragment, and the
    /// message it belongs to would then be parsed from its own middle.
    ///
    /// A first version of this test wrote two whole events and read one,
    /// believing the second would be buffered. It is not — it stays in the
    /// socket, which the reader inherits anyway — so that test passed
    /// whether the buffer moved or not.
    #[test]
    fn detaching_carries_the_bytes_already_buffered() {
        let (mut connection, mut peer) = pair();
        let whole = event(7, 0, &9u32.to_ne_bytes());
        let (head, tail) = whole.split_at(5);
        peer.write_all(head).unwrap();

        // Part of a message and no more: this read banks the fragment and
        // then times out waiting for the rest of it.
        connection
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        assert!(connection.next().is_err());

        let mut reader = connection.detach_reader().unwrap();
        peer.write_all(tail).unwrap();
        let message = reader.next().unwrap();
        assert_eq!(
            (message.object, message.opcode),
            (7, 0),
            "the fragment did not survive the detach"
        );
        assert_eq!(message.payload, 9u32.to_ne_bytes().to_vec());
    }

    /// Two readers on one socket would split a message between them, and the
    /// half each got would be unparseable in a different way. Detaching is
    /// therefore one-way and stated in the errors rather than by convention.
    #[test]
    fn only_one_half_may_read() {
        let (mut connection, mut peer) = pair();
        peer.write_all(&event(7, 0, &3u32.to_ne_bytes())).unwrap();
        let _reader = connection.detach_reader().unwrap();

        let refused = connection.next().err().unwrap();
        assert!(refused.contains("another thread"), "{refused}");
        let refused = connection.detach_reader().err().unwrap();
        assert!(refused.contains("already detached"), "{refused}");
    }

    /// Detaching before the handshake is over is refused, because
    /// `read_next` sets the socket's read timeout from a deadline and never
    /// clears it: a reader that inherited the socket after the deadline was
    /// dropped but before the timeout was would wake with `TimedOut` on a
    /// compositor that had simply said nothing.
    #[test]
    fn the_reading_half_detaches_only_after_the_handshake() {
        let (ours, _peer) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut connection = Connection::over(ours, Some(deadline), 10);
        let refused = connection.detach_reader().err().unwrap();
        assert!(
            refused.contains("before the handshake finished"),
            "{refused}"
        );

        connection.finish_handshake().unwrap();
        connection.detach_reader().unwrap();
    }

    /// Descriptors move with the reader, and the reader closes what it still
    /// holds. Both halves matter and neither is visible from the protocol: a
    /// queue left behind is a descriptor held open until the process ends,
    /// and one carried but never closed is the same leak in a new place.
    ///
    /// The socket pair is the oracle — a peer reads end-of-file only once
    /// EVERY handle on the other end is gone, so the read returning zero is
    /// the reader's `Drop` having closed the one it carried.
    #[test]
    fn descriptors_move_with_the_reader_and_are_closed_by_it() {
        let (mut connection, _peer) = pair();
        let (held, mut watcher) = UnixStream::pair().unwrap();
        connection.queue_fd_for_test(held.into_raw_fd());
        assert_eq!(connection.pending_fd_count(), 1);

        let reader = connection.detach_reader().unwrap();
        assert_eq!(
            connection.pending_fd_count(),
            0,
            "the connection kept a descriptor it had handed over"
        );
        assert_eq!(reader.pending_fd_count(), 1);

        drop(reader);
        watcher
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut byte = [0u8; 1];
        assert_eq!(
            watcher.read(&mut byte).unwrap(),
            0,
            "the reader did not close the descriptor it carried"
        );
    }

    /// Writing is what stays behind. The whole point of detaching is that the
    /// main loop keeps issuing requests while a thread blocks in a read, so a
    /// connection that could no longer write would have gained nothing.
    #[test]
    fn a_detached_connection_still_writes() {
        let (mut connection, mut peer) = pair();
        let _reader = connection.detach_reader().unwrap();
        connection.send(SURFACE, 6, wire::Builder::new()).unwrap();

        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut said = [0u8; 8];
        peer.read_exact(&mut said).unwrap();
        let object = u32::from_ne_bytes([said[0], said[1], said[2], said[3]]);
        let opcode = u16::from_ne_bytes([said[4], said[5]]);
        assert_eq!((object, opcode), (SURFACE, 6));
    }
}
