//! Bounded, nonblocking clipboard endpoints under an explicit clock: the
//! text a window offered, written to the right a `wl_data_source.send`
//! hands it (`Outgoing`), and the selection's text, read from the private
//! socket pair a `wl_data_offer.receive` is given the far end of
//! (`Incoming`). Each transfer has a five-second deadline from `begin`
//! and makes at most four 16 KiB I/O attempts a step, so a peer that
//! stalls or floods holds up no turn; a failure is terminal and no prefix
//! of a failed or unfinished transfer is ever handed on. These are
//! transport owners, not clipboard ownership: the data device, its
//! offers and the live source are the client's (`client`), and what is
//! offered or pasted is the consumer's. The widget window (`window`)
//! owns one of each; td-editor's window owns its own `Outgoing` and an
//! `Incoming` of its own that admits the paste as it arrives.
//!
//! The send's right is a pipe or socket the compositor created, whose
//! open-file description it may still share; `Outgoing` adds
//! `O_NONBLOCK` to that description's status word through the raw
//! module's two pinned `fcntl` commands (UNSAFE.md §19) and restores the
//! exact original word when it is done or cancelled, best-effort on
//! drop. The receive's endpoint is this process's own socket, made
//! nonblocking by safe `std`.

use crate::sys;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

/// The most text a transfer carries either way.
pub const MAX_BYTES: usize = 1024 * 1024;
const CHUNK: usize = 16 * 1024;
const DEADLINE_MS: u64 = 5000;
const O_NONBLOCK: usize = 0o4000;
const O_ACCMODE: usize = 3;

#[derive(Clone, Copy, Eq, PartialEq)]
enum State {
    Pending,
    Complete,
    Failed,
}

/// The received write endpoint, never a regular file or a device. Its
/// status flags are the open-file description's, shared with whoever
/// else holds it, so the exact original word is restored on close.
struct Destination {
    file: Option<File>,
    original: usize,
}

impl Destination {
    fn new(fd: OwnedFd) -> io::Result<Self> {
        let file = File::from(fd);
        let kind = file.metadata()?.file_type();
        if !kind.is_fifo() && !kind.is_socket() {
            return Err(io::Error::other(
                "clipboard destination must be a pipe or socket",
            ));
        }
        let original = sys::status(&file)?;
        if !matches!(original & O_ACCMODE, 1 | 2) {
            return Err(io::Error::other("clipboard destination is not writable"));
        }
        // The owner exists before the flag is set, so every exit after
        // it, a refused readback included, restores the original word.
        let destination = Self {
            file: Some(file),
            original,
        };
        let file = destination
            .file
            .as_ref()
            .ok_or_else(|| io::Error::other("closed clipboard destination"))?;
        sys::set_status(file, original | O_NONBLOCK)?;
        if sys::status(file)? != original | O_NONBLOCK {
            return Err(io::Error::other("clipboard nonblocking readback differs"));
        }
        Ok(destination)
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("closed clipboard destination"))?
            .write(bytes)
    }

    /// Restores the original status word with readback; std closes the
    /// descriptor after. Once.
    fn close(&mut self) -> io::Result<()> {
        let Some(file) = self.file.take() else {
            return Ok(());
        };
        sys::set_status(&file, self.original)?;
        if sys::status(&file)? != self.original {
            return Err(io::Error::other("clipboard status restore differs"));
        }
        Ok(())
    }
}

impl Drop for Destination {
    fn drop(&mut self) {
        // Best effort: a drop cannot report a restoration that failed.
        let _ = self.close();
    }
}

/// An immutable UTF-8 text and the one exclusively used pipe or socket
/// it is written to. Status flags are shared with descriptor duplicates:
/// no other owner may concurrently write or change those flags.
/// Completion and cancel restore them; drop makes the same best-effort
/// attempt but cannot report an error. As with ordinary std pipe writes,
/// a broken pipe assumes Rust's default ignored SIGPIPE disposition;
/// embedders must not restore the default signal action.
pub struct Outgoing {
    destination: Destination,
    text: Arc<str>,
    offset: usize,
    deadline: u64,
    clock: u64,
    state: State,
}

impl Outgoing {
    /// Whether a pending transfer must time out without writing more.
    pub fn expired(&self, now: u64) -> bool {
        self.state == State::Pending && now >= self.deadline
    }

    /// Owns the exact destination, refusing a non-pipe/socket or a
    /// read-only endpoint and a text past `MAX_BYTES`, and makes it
    /// nonblocking, read back, before any write.
    pub fn begin(fd: OwnedFd, text: Arc<str>, now: u64) -> io::Result<Self> {
        if text.len() > MAX_BYTES {
            return Err(io::Error::other("clipboard source byte budget"));
        }
        let deadline = now
            .checked_add(DEADLINE_MS)
            .ok_or_else(|| io::Error::other("clipboard clock exhausted"))?;
        Ok(Self {
            destination: Destination::new(fd)?,
            text,
            offset: 0,
            deadline,
            clock: now,
            state: State::Pending,
        })
    }

    /// True once every byte was written and the flags restored. Failure
    /// is permanent and closes the endpoint; the peer may have received
    /// a prefix.
    pub fn step(&mut self, now: u64) -> io::Result<bool> {
        match self.state {
            State::Complete => return Ok(true),
            State::Failed => return Err(io::Error::other("clipboard write already failed")),
            State::Pending => {}
        }
        if now < self.clock {
            return self.failed(io::Error::other("clipboard clock moved backwards"));
        }
        self.clock = now;
        if now >= self.deadline {
            return self.failed(io::Error::other("clipboard write deadline"));
        }
        for _ in 0..4 {
            if self.offset == self.text.len() {
                return self.complete();
            }
            let end = self.offset.saturating_add(CHUNK).min(self.text.len());
            let Some(bytes) = self.text.as_bytes().get(self.offset..end) else {
                return self.failed(io::Error::other("clipboard write offset"));
            };
            match self.destination.write(bytes) {
                Ok(0) => return self.failed(io::Error::other("clipboard write returned zero")),
                Ok(count) if count <= bytes.len() => self.offset += count,
                Ok(_) => return self.failed(io::Error::other("clipboard write length")),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) => return self.failed(e),
            }
        }
        if self.offset == self.text.len() {
            self.complete()
        } else {
            Ok(false)
        }
    }

    fn complete(&mut self) -> io::Result<bool> {
        match self.destination.close() {
            Ok(()) => {
                self.state = State::Complete;
                Ok(true)
            }
            Err(error) => {
                self.state = State::Failed;
                Err(io::Error::other(format!(
                    "clipboard payload sent; restoring destination failed: {error}"
                )))
            }
        }
    }

    fn failed(&mut self, error: io::Error) -> io::Result<bool> {
        self.state = State::Failed;
        match self.destination.close() {
            Ok(()) => Err(error),
            Err(restore) => Err(io::Error::other(format!(
                "{error}; restoring destination: {restore}"
            ))),
        }
    }

    /// Closes early, reporting a restoration that failed. Bytes already
    /// sent cannot be retracted.
    pub fn cancel(mut self) -> io::Result<()> {
        self.destination.close()
    }
}

/// A bounded reader of the selection's text over a private socket pair;
/// dropping it cancels the paste. The bytes are handed on only whole,
/// at EOF, as UTF-8; a text past `MAX_BYTES`, a failed read or a missed
/// deadline poisons the transfer for good.
pub struct Incoming {
    stream: UnixStream,
    bytes: Vec<u8>,
    deadline: u64,
    clock: u64,
    read: Box<[u8]>,
    state: State,
}

impl Incoming {
    /// Whether a pending transfer must time out without reading more.
    pub fn expired(&self, now: u64) -> bool {
        self.state == State::Pending && now >= self.deadline
    }

    /// The receiver and the producer endpoint to hand the selection's
    /// offer; the caller drops its own copy of that endpoint after the
    /// hand-off, so EOF can arrive.
    pub fn begin(now: u64) -> io::Result<(Self, File)> {
        let deadline = now
            .checked_add(DEADLINE_MS)
            .ok_or_else(|| io::Error::other("clipboard clock exhausted"))?;
        let (stream, peer) = UnixStream::pair()?;
        stream.set_nonblocking(true)?;
        Ok((
            Self {
                stream,
                bytes: Vec::new(),
                deadline,
                clock: now,
                read: vec![0; CHUNK].into_boxed_slice(),
                state: State::Pending,
            },
            File::from(OwnedFd::from(peer)),
        ))
    }

    /// True only after EOF. A failure is permanent.
    pub fn step(&mut self, now: u64) -> io::Result<bool> {
        match self.state {
            State::Complete => return Ok(true),
            State::Failed => return Err(io::Error::other("clipboard read already failed")),
            State::Pending => {}
        }
        match self.read(now) {
            Ok(done) => {
                if done {
                    self.state = State::Complete;
                }
                Ok(done)
            }
            Err(e) => {
                self.state = State::Failed;
                self.bytes = Vec::new();
                Err(e)
            }
        }
    }

    fn read(&mut self, now: u64) -> io::Result<bool> {
        if now < self.clock {
            return Err(io::Error::other("clipboard clock moved backwards"));
        }
        self.clock = now;
        if now >= self.deadline {
            return Err(io::Error::other("clipboard read deadline"));
        }
        // Four bounded reads per turn, Interrupted included: a peer that
        // keeps the endpoint full cannot starve the loop's own events.
        for _ in 0..4 {
            match self.stream.read(&mut self.read) {
                Ok(0) => return Ok(true),
                Ok(count) => {
                    let bytes = self
                        .read
                        .get(..count)
                        .ok_or_else(|| io::Error::other("clipboard read length"))?;
                    if self.bytes.len().saturating_add(count) > MAX_BYTES {
                        return Err(io::Error::other("clipboard paste byte budget"));
                    }
                    self.bytes.extend_from_slice(bytes);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(false)
    }

    /// The text, whole, of a transfer that reached EOF; not UTF-8 is a
    /// failure, as an unfinished transfer is.
    pub fn finish(self) -> io::Result<String> {
        if self.state != State::Complete {
            return Err(io::Error::other("clipboard transfer has no successful EOF"));
        }
        String::from_utf8(self.bytes).map_err(|_| io::Error::other("clipboard text is not UTF-8"))
    }
}
