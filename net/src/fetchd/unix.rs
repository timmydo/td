//! Explicit private pathname transport; it never resolves or falls back to TCP.
use std::{
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::Arc,
    time::Instant,
};
use td_fs::SocketPath;
use ureq::{
    http::Uri,
    unversioned::transport::{
        time::{Duration as Span, Instant as Moment},
        Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, ShutdownHandle, Transport,
    },
    Error,
};

pub(super) const MAX_ENDPOINTS: usize = 64;

pub(super) fn endpoint(text: &str) -> io::Result<SocketPath> {
    if text.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unix endpoint contains a control byte",
        ));
    }
    SocketPath::new(std::path::Path::new(text))
}
pub(super) fn check_uri(uri: &Uri) -> Result<(), Error> {
    if uri.scheme_str() != Some("http") || uri.authority().map(|a| a.as_str()) != Some("localhost")
    {
        return Err(Error::BadUri(
            "private Unix requests require http://localhost without a port".into(),
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct UnixConnector(pub SocketPath);
impl Connector for UnixConnector {
    type Out = UnixTransport;
    fn connect(
        &self,
        details: &ConnectionDetails<'_>,
        chained: Option<()>,
    ) -> Result<Option<Self::Out>, Error> {
        check_uri(details.uri)?;
        if chained.is_some() || details.config.proxy().is_some() {
            return Err(Error::ConnectionFailed);
        }
        let Moment::Exact(start) = details.now else {
            return Err(Error::ConnectionFailed);
        };
        let until = end(start, details.timeout)?;
        td_fs::check_private_socket(self.0.as_path())?;
        let stream = td_fs::unix_connect_until(self.0.as_path(), until)
            .map_err(|e| timed(e, details.timeout))?;
        td_fs::check_private_socket(self.0.as_path())?;
        let socket = Arc::new(Socket(stream));
        let shutdown: Arc<dyn ShutdownHandle> = socket.clone();
        (details.register_shutdown)(Arc::downgrade(&shutdown))?;
        Ok(Some(UnixTransport {
            socket,
            buffers: LazyBuffers::new(
                details.config.input_buffer_size(),
                details.config.output_buffer_size(),
            ),
        }))
    }
}
#[derive(Debug)]
struct Socket(UnixStream);
impl ShutdownHandle for Socket {
    fn shutdown(&self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}
#[derive(Debug)]
pub(super) struct UnixTransport {
    socket: Arc<Socket>,
    buffers: LazyBuffers,
}
fn end(now: Instant, timeout: NextTimeout) -> Result<Instant, Error> {
    let Span::Exact(left) = timeout.after else {
        return Err(Error::ConnectionFailed);
    };
    if left.is_zero() {
        return Err(Error::Timeout(timeout.reason));
    }
    now.checked_add(left).ok_or(Error::Timeout(timeout.reason))
}
fn remaining(until: Instant, timeout: NextTimeout) -> Result<std::time::Duration, Error> {
    until
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or(Error::Timeout(timeout.reason))
}
fn timed(error: io::Error, timeout: NextTimeout) -> Error {
    match error.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Error::Timeout(timeout.reason),
        _ => Error::Io(error),
    }
}
impl Transport for UnixTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        let timeout = timeout.for_write();
        let until = end(Instant::now(), timeout)?;
        let mut bytes = self
            .buffers
            .output()
            .get(..amount)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut socket = &self.socket.0;
        while !bytes.is_empty() {
            socket.set_write_timeout(Some(remaining(until, timeout)?))?;
            match socket.write(bytes) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
                Ok(count) => {
                    bytes = bytes
                        .get(count..)
                        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(timed(error, timeout)),
            }
        }
        Ok(())
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        let timeout = timeout.for_read();
        let until = end(Instant::now(), timeout)?;
        let mut socket = &self.socket.0;
        let bytes = self.buffers.input_append_buf();
        loop {
            socket.set_read_timeout(Some(remaining(until, timeout)?))?;
            match socket.read(bytes) {
                Ok(count) => {
                    self.buffers.input_appended(count);
                    return Ok(count != 0);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(timed(error, timeout)),
            }
        }
    }
    fn is_open(&mut self) -> bool {
        if !self.buffers.input().is_empty() || self.socket.0.set_nonblocking(true).is_err() {
            return false;
        }
        let mut byte = [0];
        let result = (&self.socket.0).read(&mut byte);
        let restored = self.socket.0.set_nonblocking(false);
        restored.is_ok()
            && matches!(result, Err(error) if error.kind() == io::ErrorKind::WouldBlock)
    }
}
