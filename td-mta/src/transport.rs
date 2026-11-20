//! Nonblocking TCP ownership; admission, dialing and scheduling live above it.
use crate::ports::{Error, FlushProgress, IoProgress, Transport};
use std::{
    io::{self, ErrorKind, Read, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    time::Instant,
};

pub(crate) fn retry_accept(error: &std::io::Error) -> bool {
    // Linux accept forwards pending connection errors. Resource pressure also
    // retries after a scan delay; invalid listening descriptors still fail.
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset
    ) || matches!(
        error.raw_os_error(),
        Some(12 | 23 | 24 | 64 | 71 | 92 | 95 | 100 | 101 | 105 | 112 | 113)
    )
}

/// Confirm the caller established the configured IPv6-only family policy.
#[allow(deprecated)]
pub(crate) fn ipv6_only(listener: &std::net::TcpListener) -> Result<bool, Error> {
    listener.only_v6().map_err(Error::from)
}

/// One socket operation never handles more than one application chunk.
pub const SOCKET_CHUNK: usize = 16 * 1024;

/// Exclusively owned connected socket. No cloning or raw socket access is exposed.
/// The caller supplies an admitted slot and an ordinary socket without linger;
/// this adapter neither creates a slot nor starts a listener or dial operation.
pub struct TcpTransport {
    socket: Option<TcpStream>,
    peer: SocketAddr,
    read_closed: bool,
    write_closed: bool,
    failure: Option<Error>,
}

impl TcpTransport {
    /// Consume a connected stream, set nonblocking I/O and capture its actual peer.
    /// The caller must not retain another handle to the same socket.
    pub fn from_stream(socket: TcpStream) -> Result<Self, Error> {
        let configured = (|| {
            socket.set_nonblocking(true)?;
            socket.set_nodelay(true)?;
            socket.set_read_timeout(None)?;
            socket.set_write_timeout(None)?;
            socket.peer_addr()
        })();
        match configured {
            Ok(peer) => Ok(Self {
                socket: Some(socket),
                peer,
                read_closed: false,
                write_closed: false,
                failure: None,
            }),
            Err(error) => {
                let _ = socket.shutdown(Shutdown::Both);
                Err(error.into())
            }
        }
    }

    /// Transport origin only; this address does not grant gateway authorization.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    /// Non-consuming, nonblocking probe; any queued byte or closure forbids reuse.
    pub(crate) fn idle_boundary(&mut self) -> Result<bool, Error> {
        if self.read_closed || self.write_closed {
            return Ok(false);
        }
        match self.socket()?.peek(&mut [0]) {
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(true),
            Ok(_) => Ok(false),
            Err(error) if error.kind() == ErrorKind::Interrupted => Ok(false),
            Err(error) => Err(self.fail(error.into())),
        }
    }

    pub(crate) fn connected(&self) -> Result<&TcpStream, Error> {
        self.socket
            .as_ref()
            .ok_or(self.failure.unwrap_or(Error::Invalid))
    }

    /// A scheduler slice may expire without retiring a healthy connection.
    pub(crate) fn wait_readable_slice(&mut self, until: Instant) -> Result<bool, Error> {
        use crate::readiness_sys::{self, Connected, Interest, Ready};
        readiness_sys::wait(
            Connected::Tcp(self.connected()?),
            Interest {
                read: true,
                write: false,
            },
            None,
            until,
        )
        .map(|ready| !matches!(ready, Ready::Timeout))
        .map_err(|error| self.fail(error.into()))
    }

    fn socket(&mut self) -> Result<&mut TcpStream, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.socket.as_mut().ok_or(Error::Invalid)
    }

    fn fail(&mut self, error: Error) -> Error {
        let error = *self.failure.get_or_insert(error);
        if let Some(socket) = self.socket.take() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        error
    }

    fn progress(&mut self, result: io::Result<usize>, reading: bool) -> Result<IoProgress, Error> {
        match result {
            Ok(0) if reading => {
                self.read_closed = true;
                Ok(IoProgress::Closed)
            }
            Ok(0) => Err(self.fail(Error::Io {
                kind: ErrorKind::WriteZero,
                os_code: None,
            })),
            Ok(count) => Ok(IoProgress::Bytes(count)),
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
            {
                Ok(IoProgress::Pending)
            }
            Err(error) => Err(self.fail(error.into())),
        }
    }
}

impl Transport for TcpTransport {
    fn read(&mut self, output: &mut [u8]) -> Result<IoProgress, Error> {
        if output.is_empty() {
            return Ok(IoProgress::Pending);
        }
        let count = output.len().min(SOCKET_CHUNK);
        let output = output.get_mut(..count).ok_or(Error::Invalid)?;
        let read_closed = self.read_closed;
        let socket = self.socket()?;
        if read_closed {
            return Ok(IoProgress::Closed);
        }
        let result = socket.read(output);
        self.progress(result, true)
    }

    fn write(&mut self, input: &[u8]) -> Result<IoProgress, Error> {
        if input.is_empty() {
            return Ok(IoProgress::Pending);
        }
        self.socket()?;
        if self.write_closed {
            return Err(self.fail(Error::Io {
                kind: ErrorKind::BrokenPipe,
                os_code: None,
            }));
        }
        let input = input
            .get(..input.len().min(SOCKET_CHUNK))
            .ok_or(Error::Invalid)?;
        let result = self.socket()?.write(input);
        self.progress(result, false)
    }

    fn flush(&mut self) -> Result<FlushProgress, Error> {
        self.socket()?;
        // TcpStream has no userspace output buffer; this is not a TCP acknowledgement.
        Ok(FlushProgress::Complete)
    }

    fn close(&mut self) -> Result<FlushProgress, Error> {
        self.socket()?;
        if self.write_closed {
            return Ok(FlushProgress::Complete);
        }
        match self.socket()?.shutdown(Shutdown::Write) {
            Ok(()) => {
                self.write_closed = true;
                Ok(FlushProgress::Complete)
            }
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
            {
                Ok(FlushProgress::Pending)
            }
            Err(error) => Err(self.fail(error.into())),
        }
    }

    fn abort(&mut self) {
        self.fail(Error::Invalid);
    }
}

impl Drop for TcpTransport {
    fn drop(&mut self) {
        self.abort();
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
