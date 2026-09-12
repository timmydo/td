//! The Wayland client transport td-owned programs share: the display
//! endpoint, a connection that frames requests, carries at most one
//! descriptor per send and owns every received right until an event's
//! consumer takes it, the unlinked private file a SHM pool is built on,
//! and the pointer image every consumer shows. `client` builds the object
//! table, the surface lifecycle and the turn loop over it. Errors are
//! strings, as the wire codec's are; a consumer maps them into its own
//! diagnostic.

use crate::sys;
use crate::wire::{self, Builder, Message};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, String>;

/// One receive fills at most `READ_BYTES`; unparsed bytes wait in a queue
/// of at most `PENDING_BYTES`; at most `DESCRIPTORS` received rights wait
/// for the events that consume them.
pub const READ_BYTES: usize = 16 * 1024;
pub const PENDING_BYTES: usize = 128 * 1024;
pub const DESCRIPTORS: usize = 8;
/// Every request has this absolute write deadline, retries included,
/// capped by the startup deadline while one is set.
pub const WRITE_DEADLINE: Duration = Duration::from_secs(5);
/// A path connection attempt is abandoned after this long.
pub const CONNECT_DEADLINE: Duration = Duration::from_secs(5);
/// The idle reader's wait when nothing else is due.
pub const IDLE_WAIT: Duration = Duration::from_millis(100);
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn error(value: impl std::fmt::Display) -> String {
    value.to_string()
}

/// Where the display is: an inherited socket descriptor, or a socket path.
#[derive(Debug, Eq, PartialEq)]
pub enum Endpoint {
    Path(PathBuf),
    Inherited(i32),
}

/// `WAYLAND_SOCKET` wins; an absolute `WAYLAND_DISPLAY` is used as is; a
/// relative one joins `XDG_RUNTIME_DIR`, which must then be absolute.
pub fn endpoint(
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

/// An inherited descriptor is duplicated close-on-exec, never adopted; a
/// path is connected by one bounded worker whose late result is dropped.
pub fn connect(endpoint: Endpoint) -> Result<UnixStream> {
    match endpoint {
        Endpoint::Inherited(fd) => sys::inherited(fd).map_err(error),
        Endpoint::Path(path) => {
            // A full Unix listen queue can block connect. One worker owns the
            // attempt; if the deadline wins, any eventual stream is dropped.
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            std::thread::Builder::new()
                .name("wayland-connect".into())
                .spawn(move || {
                    let _ = sender.send(UnixStream::connect(path));
                })
                .map_err(error)?;
            receiver
                .recv_timeout(CONNECT_DEADLINE)
                .map_err(|e| format!("Wayland connect: {e}"))?
                .map_err(error)
        }
    }
}

/// One display connection: the stream, the bytes received but not yet
/// parsed, the rights received but not yet consumed, the startup deadline
/// that caps every wait until the first frame is submitted, and the idle
/// wait the consumer sets from its own schedule.
pub struct Connection {
    stream: UnixStream,
    pending: Vec<u8>,
    read: [u8; READ_BYTES],
    startup_deadline: Option<Instant>,
    descriptors: VecDeque<OwnedFd>,
    wait: Duration,
}

impl Connection {
    pub fn new(stream: UnixStream) -> Result<Self> {
        stream.set_read_timeout(Some(IDLE_WAIT)).map_err(error)?;
        Ok(Self {
            stream,
            pending: Vec::with_capacity(PENDING_BYTES),
            read: [0; READ_BYTES],
            startup_deadline: None,
            descriptors: VecDeque::with_capacity(DESCRIPTORS),
            wait: IDLE_WAIT,
        })
    }

    /// Sends one request, with at most one borrowed file as its right. A
    /// short write transfers the right once; only bytes are retried.
    pub fn send(
        &mut self,
        object: u32,
        opcode: u16,
        body: Builder,
        file: Option<&File>,
    ) -> Result<()> {
        let bytes = body.message(object, opcode)?;
        let deadline = Instant::now() + self.budget(WRITE_DEADLINE)?;
        let mut offset = 0;
        let mut file = file;
        while offset < bytes.len() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .ok_or("Wayland write deadline")?;
            self.stream
                .set_write_timeout(Some(remaining))
                .map_err(error)?;
            let suffix = bytes.get(offset..).ok_or("Wayland write offset")?;
            let sent = if let Some(right) = file {
                sys::send_file(&self.stream, suffix, right)
            } else {
                self.stream.write(suffix)
            };
            match sent {
                Ok(0) => return Err("Wayland write returned zero".into()),
                Ok(count) if count <= suffix.len() => {
                    offset += count;
                    file = None;
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

    /// Sends a request whose body is whole words.
    pub fn words(&mut self, object: u32, opcode: u16, words: &[u32]) -> Result<()> {
        let mut body = Builder::new();
        for word in words {
            body.u32(*word);
        }
        self.send(object, opcode, body, None)
    }

    /// The next complete event, if the pending bytes hold one.
    pub fn take(&mut self) -> Result<Option<Message>> {
        wire::take(&mut self.pending)
    }

    /// Waits at most the idle wait (capped by the startup deadline) for
    /// more bytes and rights, refusing either budget's overflow.
    pub fn read_more(&mut self) -> Result<()> {
        let wait = self.budget(self.wait)?;
        self.stream.set_read_timeout(Some(wait)).map_err(error)?;
        let start = Instant::now();
        match sys::receive(&self.stream, &mut self.read) {
            Ok((0, _)) => Err("Wayland compositor disconnected".into()),
            Ok((count, fds)) => {
                if self.pending.len().saturating_add(count) > PENDING_BYTES
                    || self.descriptors.len().saturating_add(fds.len()) > DESCRIPTORS
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

    /// `limit`, or what is left of the startup deadline if that is less;
    /// an expired startup deadline is the error.
    pub fn budget(&self, limit: Duration) -> Result<Duration> {
        match self.startup_deadline {
            Some(deadline) => deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .map(|d| d.min(limit))
                .ok_or("Wayland initial commit deadline".into()),
            None => Ok(limit),
        }
    }

    /// The oldest received right not yet consumed, if any; an event's
    /// consumer pops its own. Nothing else reorders or extends the FIFO.
    pub fn pop_descriptor(&mut self) -> Option<OwnedFd> {
        self.descriptors.pop_front()
    }

    /// How many received rights wait for their events, at most
    /// `DESCRIPTORS`.
    pub fn descriptors(&self) -> usize {
        self.descriptors.len()
    }

    pub fn wait(&self) -> Duration {
        self.wait
    }

    pub fn set_wait(&mut self, wait: Duration) {
        self.wait = wait;
    }

    pub fn startup_deadline(&self) -> Option<Instant> {
        self.startup_deadline
    }

    /// Until the first frame is submitted every wait and write is capped
    /// by this deadline; clearing it lets an occluded surface wait.
    pub fn set_startup_deadline(&mut self, deadline: Option<Instant>) {
        self.startup_deadline = deadline;
    }
}

/// A private, unlinked, 0600 regular file of `size` bytes for a SHM pool:
/// `create_new` under a checked process-local serial, unlinked before it
/// is sized, reachable afterwards only through the returned owner.
pub fn backing_file(directory: &Path, size: usize) -> Result<File> {
    for _ in 0..64 {
        let serial = NEXT_FILE
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| "pool name counter exhausted")?;
        let path = directory.join(format!(".td-ui-shm-{}-{serial}", std::process::id()));
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
                ));
            }
        }
    }
    Err("Wayland pool filename collision budget".into())
}

pub const CURSOR_WIDTH: usize = 16;
pub const CURSOR_HEIGHT: usize = 24;

/// The pointer image: a 16x24 ARGB8888 arrow in the palette's ink and a
/// paper-white fill, transparent elsewhere.
pub fn cursor_pixels() -> [u8; CURSOR_WIDTH * CURSOR_HEIGHT * 4] {
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
    let mut pixels = [0; CURSOR_WIDTH * CURSOR_HEIGHT * 4];
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

/// The scripted peer's side of a socket pair, for a consumer's tests: what
/// the client sent, as events and as the files it passed, and a right
/// delivered to the client without a socket.
pub mod peer {
    use super::{sys, wire, Connection, Message, Result, DESCRIPTORS, PENDING_BYTES, READ_BYTES};
    use std::fs::File;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    /// Queues one right as if the compositor had sent it, under the same
    /// bound the reader enforces.
    pub fn push_descriptor(connection: &mut Connection, fd: OwnedFd) -> Result<()> {
        if connection.descriptors.len() >= DESCRIPTORS {
            return Err("Wayland receive budget".into());
        }
        connection.descriptors.push_back(fd);
        Ok(())
    }

    /// Reads until the client's side goes quiet, then parses every complete
    /// request; a partial trailing request is an error. Public because a
    /// consumer's tests are another crate, so it is held to the connection's
    /// own byte and right budgets. A blocking stream without a read timeout
    /// is given the idle wait, so the reader always returns; a nonblocking
    /// one returns as soon as nothing is queued.
    pub fn drain(stream: &UnixStream) -> Result<(Vec<Message>, Vec<File>)> {
        if stream.read_timeout().map_err(super::error)?.is_none() {
            stream
                .set_read_timeout(Some(super::IDLE_WAIT))
                .map_err(super::error)?;
        }
        let mut bytes = Vec::new();
        let mut files = Vec::new();
        let mut buffer = [0; READ_BYTES];
        loop {
            match sys::receive(stream, &mut buffer) {
                Ok((0, _)) => break,
                Ok((count, fds)) => {
                    if bytes.len().saturating_add(count) > PENDING_BYTES
                        || files.len().saturating_add(fds.len()) > DESCRIPTORS
                    {
                        return Err("peer receive budget".into());
                    }
                    bytes.extend_from_slice(buffer.get(..count).ok_or("peer receive length")?);
                    files.extend(fds.into_iter().map(File::from));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(e) => return Err(format!("peer receive: {e}")),
            }
        }
        let mut messages = Vec::new();
        while let Some(message) = wire::take(&mut bytes)? {
            messages.push(message);
        }
        if !bytes.is_empty() {
            return Err("peer received a partial request".into());
        }
        Ok((messages, files))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;

    /// xdg_wm_base's fixed id as a consumer lays it out; pong is its opcode 3.
    const WM: u32 = 6;

    fn message(object: u32, opcode: u16, words: &[u32]) -> Message {
        let mut b = Builder::new();
        for w in words {
            b.u32(*w);
        }
        wire::take(&mut b.message(object, opcode).unwrap())
            .unwrap()
            .unwrap()
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
            if n < DESCRIPTORS {
                receiver.read_more().unwrap();
            } else {
                assert!(receiver.read_more().unwrap_err().contains("budget"));
            }
            endpoints.push(peer);
        }
        assert_eq!(receiver.descriptors(), DESCRIPTORS);
        drop(receiver);
        for mut peer in endpoints {
            assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        }
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
        connection.set_startup_deadline(Some(Instant::now() + Duration::from_millis(25)));
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

    /// The peer reads back what a connection sent, requests and rights
    /// alike; a pool file is private, unlinked and exactly sized.
    #[test]
    fn peers_drain_requests_with_their_rights_and_pools_are_unlinked() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut connection = Connection::new(a).unwrap();
        let pool = backing_file(&std::env::temp_dir(), 4096).unwrap();
        let meta = pool.metadata().unwrap();
        assert_eq!(meta.len(), 4096);
        assert_eq!(meta.nlink(), 0, "pool file is unlinked");
        assert_eq!(meta.mode() & 0o777, 0o600);
        connection.words(1, 1, &[2]).unwrap();
        let mut body = Builder::new();
        body.u32(11);
        body.u32(4096);
        connection.send(5, 0, body, Some(&pool)).unwrap();
        let (messages, files) = peer::drain(&b).unwrap();
        assert_eq!(messages, [message(1, 1, &[2]), message(5, 0, &[11, 4096])]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].metadata().unwrap().ino(), meta.ino());
        assert_eq!(
            b.read_timeout().unwrap(),
            Some(IDLE_WAIT),
            "a timeout was given"
        );
        let (endpoint, _far) = UnixStream::pair().unwrap();
        let mut rights = Connection::new(b).unwrap();
        for n in 0..DESCRIPTORS {
            let (_peer, right) = UnixStream::pair().unwrap();
            peer::push_descriptor(&mut rights, right.into()).unwrap();
            assert_eq!(rights.descriptors(), n + 1);
        }
        assert!(peer::push_descriptor(&mut rights, endpoint.into())
            .unwrap_err()
            .contains("budget"));
        assert!(rights.pop_descriptor().is_some());
        assert_eq!(rights.descriptors(), DESCRIPTORS - 1);
        assert_eq!(cursor_pixels().len(), CURSOR_WIDTH * CURSOR_HEIGHT * 4);
        assert_eq!(
            &cursor_pixels()[..8],
            &[0x3f, 0x45, 0x48, 0xff, 0, 0, 0, 0],
            "an ink pixel then a transparent one on the first row"
        );
    }
}
