//! td-audio's Unix socket as the client's [`Transport`], with a deadline.
//!
//! The one file of this crate that names an operating-system facility, and
//! only these: a connected Unix stream, the path it connects to, the io
//! traits and errors it reads and writes through, and a monotonic clock for
//! its deadline. Every td controller — the `td-audio` CLI, the compositor's
//! volume keys and bar, later the mixer panel — reaches the daemon this way,
//! so the code lives here once rather than in a copy per consumer. The rest
//! of the crate stays bytes in and bytes out.
//!
//! The deadline bounds whole requests, not single calls: each read and each
//! partial write re-arms the socket's timeout with what is left of it, so a
//! server that answers, or reads, a byte at a time cannot stretch a request
//! past it. Connecting is not bounded by it: a Unix socket connect waits,
//! with no timeout, while the listener's backlog is full, so a daemon that
//! has stopped accepting holds a connect until it accepts or goes away.

use crate::client::Transport;
use std::io::{Error, ErrorKind, Read, Result, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// The most one write asks of the kernel. A blocking Unix-stream write
/// loops in the kernel until all of it is queued, restarting the send
/// timeout at each wait for buffer space, so a large one can outlast the
/// deadline in one call; a write smaller than the socket's buffer waits at
/// most once, and re-arming between writes bounds the whole send.
const WRITE_MAX: usize = 64 << 10;

pub struct UnixSocket {
    stream: UnixStream,
    deadline: Option<Instant>,
}

impl UnixSocket {
    /// Connect to the socket at `path`, with no deadline yet.
    pub fn connect(path: &Path) -> Result<Self> {
        Self::from_stream(UnixStream::connect(path)?)
    }

    /// A transport over a stream already connected, made blocking: the
    /// deadline is a timeout on blocking calls, and a nonblocking stream's
    /// `WouldBlock` would read as the deadline passing. Blocking is a flag
    /// of the open socket, so a `try_clone` the caller kept is made blocking
    /// too.
    pub fn from_stream(stream: UnixStream) -> Result<Self> {
        stream.set_nonblocking(false)?;
        Ok(Self {
            stream,
            deadline: None,
        })
    }

    /// Every send and receive from now on must be done within `within`.
    /// A deadline past what an `Instant` can hold is now: it fails closed.
    pub fn deadline_in(&mut self, within: Duration) {
        let now = Instant::now();
        self.deadline = Some(now.checked_add(within).unwrap_or(now));
    }

    /// Wait as long as the server takes, as a subscriber waits for events.
    pub fn no_deadline(&mut self) {
        self.deadline = None;
    }

    /// Arm one of the socket's timeouts with what is left of the deadline.
    fn arm(&self, set: fn(&UnixStream, Option<Duration>) -> Result<()>) -> Result<()> {
        let Some(deadline) = self.deadline else {
            return set(&self.stream, None);
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ErrorKind::TimedOut.into());
        }
        set(&self.stream, Some(left))
    }
}

/// A timed-out socket call, as the one error it means.
fn timed_out(error: Error) -> Error {
    match error.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => ErrorKind::TimedOut.into(),
        _ => error,
    }
}

impl Transport for UnixSocket {
    type Error = Error;

    fn send(&mut self, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            self.arm(UnixStream::set_write_timeout)?;
            match self.stream.write(bytes.get(..WRITE_MAX).unwrap_or(bytes)) {
                Ok(0) => return Err(ErrorKind::WriteZero.into()),
                Ok(written) => bytes = bytes.get(written..).unwrap_or_default(),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(timed_out(error)),
            }
        }
        Ok(())
    }

    fn receive(&mut self, buffer: &mut [u8]) -> Result<usize> {
        loop {
            self.arm(UnixStream::set_read_timeout)?;
            match self.stream.read(buffer) {
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                result => return result.map_err(timed_out),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::thread;

    /// A deadline ends a wait on time, however the server behaves; without
    /// one, a receive waits for what is sent.
    #[test]
    fn the_deadline_bounds_a_wait_and_its_absence_does_not() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut socket = UnixSocket::from_stream(ours).unwrap();
        socket.deadline_in(Duration::from_millis(100));
        let started = Instant::now();
        let mut buffer = [0u8; 8];
        let error = socket.receive(&mut buffer).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        // Spent: the next call fails before it waits.
        assert_eq!(socket.send(&[1]).unwrap_err().kind(), ErrorKind::TimedOut);
        socket.no_deadline();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            theirs.write_all(b"late").unwrap();
            theirs
        });
        assert_eq!(socket.receive(&mut buffer).unwrap(), 4);
        assert_eq!(&buffer[..4], b"late");
        drop(writer.join().unwrap());
        socket.deadline_in(Duration::from_secs(5));
        assert_eq!(socket.receive(&mut buffer).unwrap(), 0, "closed");
        // A deadline no `Instant` can hold is already spent.
        socket.deadline_in(Duration::MAX);
        assert_eq!(
            socket.receive(&mut buffer).unwrap_err().kind(),
            ErrorKind::TimedOut
        );
    }

    /// A server that keeps sending, a byte at a time, inside every single
    /// read's patience, still cannot hold a request past the deadline.
    #[test]
    fn a_drip_cannot_outlast_the_deadline() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let drip = thread::spawn(move || {
            for _ in 0..100 {
                if theirs.write_all(&[0]).is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let mut socket = UnixSocket::from_stream(ours).unwrap();
        socket.deadline_in(Duration::from_millis(300));
        let started = Instant::now();
        let mut buffer = [0u8; 1];
        let outcome = loop {
            match socket.receive(&mut buffer) {
                Ok(0) => break Ok(()),
                Ok(_) => {}
                Err(error) => break Err(error.kind()),
            }
        };
        assert_eq!(outcome, Err(ErrorKind::TimedOut));
        // Wide of the 300 ms deadline for a loaded host, and still well
        // short of the two seconds the drip runs.
        assert!(started.elapsed() < Duration::from_millis(1_500));
        drop(socket);
        drip.join().unwrap();
    }

    /// A server that reads a little at a time cannot hold a send past the
    /// deadline either: each partial write is armed with what is left. The
    /// reader frees enough, often enough, that no single write waits out
    /// the deadline, so only the whole send's bound ends it.
    #[test]
    fn a_slow_reader_cannot_outlast_the_deadline() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let reader = thread::spawn(move || {
            let mut sip = vec![0u8; 64 << 10];
            for _ in 0..200 {
                if matches!(theirs.read(&mut sip), Ok(0) | Err(_)) {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        let mut socket = UnixSocket::from_stream(ours).unwrap();
        socket.deadline_in(Duration::from_millis(300));
        let started = Instant::now();
        let big = vec![0u8; 4 << 20];
        assert_eq!(socket.send(&big).unwrap_err().kind(), ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(1_500));
        drop(socket);
        reader.join().unwrap();
    }

    /// A nonblocking stream is made blocking, so an empty read waits for its
    /// deadline rather than reporting it passed at once.
    #[test]
    fn a_nonblocking_stream_is_made_blocking() {
        let (ours, _theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        let mut socket = UnixSocket::from_stream(ours).unwrap();
        socket.deadline_in(Duration::from_millis(200));
        let started = Instant::now();
        let mut buffer = [0u8; 1];
        assert_eq!(
            socket.receive(&mut buffer).unwrap_err().kind(),
            ErrorKind::TimedOut
        );
        assert!(started.elapsed() >= Duration::from_millis(150));
    }
}
