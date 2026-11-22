//! The byte stream a client runs over, and the plain sockets it opens.
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// Why a stream operation stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamError {
    /// The deadline passed first.
    TimedOut,
    /// The transport failed.
    Failed,
}

/// A connected, ordered byte stream with deadlines. The client is its
/// one user and calls it from one thread.
pub trait Stream {
    /// Read at least one byte into `buffer`, or return 0 once the peer has
    /// closed, waiting no later than `deadline`. A deadline already past
    /// reads only what is ready now. `buffer` is never empty, and the
    /// count returned is at most its length.
    fn read(&mut self, buffer: &mut [u8], deadline: Instant) -> Result<usize, StreamError>;

    /// Write all of `bytes` by `deadline`. After an error an unknown
    /// prefix of `bytes` may have been written.
    fn write_all(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), StreamError>;
}

/// A TLS client the caller supplies for TCP connections. The server has
/// already accepted the SSLRequest and nothing past its one-byte answer
/// has been read, so the handshake starts on a clean socket.
pub trait TlsConnect {
    type Stream: Stream;

    /// Run the handshake over `socket` by `deadline`, verifying the
    /// server's chain and name. The socket is blocking; its read and write
    /// timeouts may be set and are the connector's to change.
    fn connect(self, socket: TcpStream, deadline: Instant) -> Result<Self::Stream, StreamError>;
}

/// A plain socket: a Unix-domain connection, or a TCP one only until its
/// TLS request is answered.
#[derive(Debug)]
pub struct Socket(Kind);

#[derive(Debug)]
enum Kind {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Socket {
    pub(crate) fn unix(stream: UnixStream) -> Self {
        Self(Kind::Unix(stream))
    }

    pub(crate) fn tcp(stream: TcpStream) -> Self {
        Self(Kind::Tcp(stream))
    }

    pub(crate) fn into_tcp(self) -> Option<TcpStream> {
        match self.0 {
            Kind::Tcp(stream) => Some(stream),
            Kind::Unix(_) => None,
        }
    }
}

impl Stream for Socket {
    fn read(&mut self, buffer: &mut [u8], deadline: Instant) -> Result<usize, StreamError> {
        match &mut self.0 {
            Kind::Unix(stream) => read_until(stream, buffer, deadline),
            Kind::Tcp(stream) => read_until(stream, buffer, deadline),
        }
    }

    fn write_all(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), StreamError> {
        match &mut self.0 {
            Kind::Unix(stream) => write_until(stream, bytes, deadline),
            Kind::Tcp(stream) => write_until(stream, bytes, deadline),
        }
    }
}

/// What the two socket types share for deadline-bounded I/O.
trait Timed: Read + Write {
    fn read_timeout(&self, wait: Option<Duration>) -> io::Result<()>;
    fn write_timeout(&self, wait: Option<Duration>) -> io::Result<()>;
    fn nonblocking(&self, on: bool) -> io::Result<()>;
}

impl Timed for UnixStream {
    fn read_timeout(&self, wait: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(wait)
    }
    fn write_timeout(&self, wait: Option<Duration>) -> io::Result<()> {
        self.set_write_timeout(wait)
    }
    fn nonblocking(&self, on: bool) -> io::Result<()> {
        self.set_nonblocking(on)
    }
}

impl Timed for TcpStream {
    fn read_timeout(&self, wait: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(wait)
    }
    fn write_timeout(&self, wait: Option<Duration>) -> io::Result<()> {
        self.set_write_timeout(wait)
    }
    fn nonblocking(&self, on: bool) -> io::Result<()> {
        self.set_nonblocking(on)
    }
}

/// The time left before `deadline`, or `None` once it has passed.
pub(crate) fn remaining(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|wait| !wait.is_zero())
}

fn read_until<T: Timed>(
    stream: &mut T,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<usize, StreamError> {
    if buffer.is_empty() {
        return Err(StreamError::Failed);
    }
    loop {
        let wait = remaining(deadline);
        let result = match wait {
            None => {
                stream.nonblocking(true).map_err(|_| StreamError::Failed)?;
                let result = stream.read(buffer);
                stream.nonblocking(false).map_err(|_| StreamError::Failed)?;
                result
            }
            Some(wait) => {
                stream
                    .read_timeout(Some(wait))
                    .map_err(|_| StreamError::Failed)?;
                stream.read(buffer)
            }
        };
        match result {
            Ok(count) if count <= buffer.len() => return Ok(count),
            Ok(_) => return Err(StreamError::Failed),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                // A wait that ended early goes round again; a poll does not.
                if wait.is_none() || remaining(deadline).is_none() {
                    return Err(StreamError::TimedOut);
                }
            }
            Err(_) => return Err(StreamError::Failed),
        }
    }
}

fn write_until<T: Timed>(
    stream: &mut T,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), StreamError> {
    let mut rest = bytes;
    while !rest.is_empty() {
        let wait = remaining(deadline).ok_or(StreamError::TimedOut)?;
        stream
            .write_timeout(Some(wait))
            .map_err(|_| StreamError::Failed)?;
        match stream.write(rest) {
            Ok(0) => return Err(StreamError::Failed),
            Ok(count) => rest = rest.get(count..).ok_or(StreamError::Failed)?,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return Err(StreamError::Failed),
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_read_waits_for_its_deadline_and_a_poll_does_not() {
        let (near, far) = UnixStream::pair().unwrap();
        let mut socket = Socket::unix(near);
        let mut byte = [0u8; 1];
        let started = Instant::now();
        assert_eq!(
            socket.read(&mut byte, started + Duration::from_millis(50)),
            Err(StreamError::TimedOut)
        );
        assert!(started.elapsed() >= Duration::from_millis(50));
        let polled = Instant::now();
        assert_eq!(socket.read(&mut byte, polled), Err(StreamError::TimedOut));
        assert!(polled.elapsed() < Duration::from_millis(50));
        (&far).write_all(b"x").unwrap();
        assert_eq!(socket.read(&mut byte, Instant::now()), Ok(1));
        assert_eq!(&byte, b"x");
        drop(far);
        assert_eq!(
            socket.read(&mut byte, Instant::now() + Duration::from_secs(1)),
            Ok(0)
        );
    }

    #[test]
    fn a_write_the_peer_never_reads_times_out() {
        let (near, _far) = UnixStream::pair().unwrap();
        let mut socket = Socket::unix(near);
        let big = vec![0u8; 16 << 20];
        let started = Instant::now();
        assert_eq!(
            socket.write_all(&big, started + Duration::from_millis(100)),
            Err(StreamError::TimedOut)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(socket.write_all(b"", Instant::now()), Ok(()));
    }
}
