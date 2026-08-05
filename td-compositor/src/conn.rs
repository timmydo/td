//! The Wayland client transport, and the object-id map its users share.
//!
//! Extracted from the demo client so the terminal is a second USER of one
//! connection rather than a second copy of it. Nothing here knows what is
//! being drawn: it connects, allocates and recycles object ids, frames
//! messages, carries descriptors, and answers the three events every client
//! must answer identically — the display's protocol error, its `delete_id`,
//! and the shell's ping.

use crate::{sys, wire};
use std::collections::{BTreeSet, VecDeque};
use std::fs::File;
use std::io::Write;
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
pub const FIRST_DYNAMIC_ID: u32 = 13;

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
    pub fn connect(path: &Path, deadline: Instant) -> Result<Connection, String> {
        let mut last = None;
        for attempt in 0..CONNECT_ATTEMPTS {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|duration| !duration.is_zero())
                .ok_or_else(|| "Wayland presentation handshake timed out".to_string())?;
            match UnixStream::connect(path) {
                Ok(stream) => return Ok(Connection::over(stream, Some(deadline))),
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
    pub fn over(stream: UnixStream, deadline: Option<Instant>) -> Connection {
        Connection {
            stream,
            buffered: Vec::with_capacity(16 * 1024),
            deadline,
            next_id: FIRST_DYNAMIC_ID,
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
            if id >= FIRST_DYNAMIC_ID {
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
