//! Admitted Unix stream I/O. Endpoint authority and dialing live above this layer.
use crate::{
    ports::{Error, FlushProgress, IoProgress, Transport},
    transport::SOCKET_CHUNK,
};
use std::{
    io::{self, ErrorKind, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    time::Instant,
};

pub struct UnixTransport {
    socket: Option<UnixStream>,
    prefetched: Option<u8>,
    read_closed: bool,
    write_closed: bool,
    failure: Option<Error>,
}
impl UnixTransport {
    pub fn from_stream(socket: UnixStream) -> Result<Self, Error> {
        socket.set_nonblocking(true)?;
        socket.set_read_timeout(None)?;
        socket.set_write_timeout(None)?;
        Ok(Self {
            socket: Some(socket),
            prefetched: None,
            read_closed: false,
            write_closed: false,
            failure: None,
        })
    }
    fn socket(&mut self) -> Result<&mut UnixStream, Error> {
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
    pub(crate) fn wait_readable(&mut self, until: Instant) -> Result<(), Error> {
        if self.wait_readable_slice(until)? {
            Ok(())
        } else {
            Err(self.fail(Error::Deadline))
        }
    }
    pub(crate) fn wait_readable_slice(&mut self, until: Instant) -> Result<bool, Error> {
        self.socket()?;
        if self.prefetched.is_some() || self.read_closed {
            return Ok(true);
        }
        let mut byte = [0];
        let Some(count) = self.blocking_slice(until, true, |socket| socket.read(&mut byte))? else {
            return Ok(false);
        };
        if count == 0 {
            self.read_closed = true;
        } else {
            self.prefetched = byte.first().copied();
        }
        Ok(true)
    }
    pub(crate) fn write_until(
        &mut self,
        input: &[u8],
        until: Instant,
    ) -> Result<IoProgress, Error> {
        if input.is_empty() {
            return Ok(IoProgress::Pending);
        }
        if self.write_closed {
            return self.write(input);
        }
        let bytes = input
            .get(..input.len().min(SOCKET_CHUNK))
            .ok_or(Error::Invalid)?;
        let count = self.blocking(until, false, |socket| socket.write(bytes))?;
        self.progress(Ok(count), false)
    }
    fn blocking(
        &mut self,
        until: Instant,
        reading: bool,
        operation: impl FnMut(&mut UnixStream) -> io::Result<usize>,
    ) -> Result<usize, Error> {
        self.blocking_slice(until, reading, operation)?
            .ok_or_else(|| self.fail(Error::Deadline))
    }
    fn blocking_slice(
        &mut self,
        until: Instant,
        reading: bool,
        mut operation: impl FnMut(&mut UnixStream) -> io::Result<usize>,
    ) -> Result<Option<usize>, Error> {
        let result = (|| {
            let socket = self.socket()?;
            socket.set_nonblocking(false)?;
            loop {
                let Some(allowance) = until
                    .checked_duration_since(Instant::now())
                    .filter(|left| !left.is_zero())
                else {
                    return Ok(None);
                };
                if reading {
                    socket.set_read_timeout(Some(allowance))?;
                } else {
                    socket.set_write_timeout(Some(allowance))?;
                }
                match operation(socket) {
                    Ok(count) => return Ok(Some(count)),
                    Err(error)
                        if matches!(
                            error.kind(),
                            ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        })();
        let restored = (|| {
            let socket = self.socket()?;
            socket.set_nonblocking(true)?;
            socket.set_read_timeout(None)?;
            socket.set_write_timeout(None)?;
            Ok(())
        })();
        match result.and_then(|count| restored.map(|()| count)) {
            Ok(count) => Ok(count),
            Err(error) => Err(self.fail(error)),
        }
    }
    fn progress(&mut self, result: io::Result<usize>, reading: bool) -> Result<IoProgress, Error> {
        match result {
            Ok(0) if reading => {
                self.read_closed = true;
                Ok(IoProgress::Closed)
            }
            Ok(0) => Err(self.fail(io::Error::from(io::ErrorKind::WriteZero).into())),
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
impl Transport for UnixTransport {
    fn read(&mut self, output: &mut [u8]) -> Result<IoProgress, Error> {
        self.socket()?;
        if output.is_empty() {
            return Ok(IoProgress::Pending);
        }
        if let Some(byte) = self.prefetched.take() {
            *output.first_mut().ok_or(Error::Invalid)? = byte;
            return Ok(IoProgress::Bytes(1));
        }
        if self.read_closed {
            return Ok(IoProgress::Closed);
        }
        let count = output.len().min(SOCKET_CHUNK);
        let output = output.get_mut(..count).ok_or(Error::Invalid)?;
        let result = self.socket()?.read(output);
        self.progress(result, true)
    }
    fn write(&mut self, input: &[u8]) -> Result<IoProgress, Error> {
        self.socket()?;
        if input.is_empty() {
            return Ok(IoProgress::Pending);
        }
        if self.write_closed {
            return Err(self.fail(io::Error::from(io::ErrorKind::BrokenPipe).into()));
        }
        let bytes = input
            .get(..input.len().min(SOCKET_CHUNK))
            .ok_or(Error::Invalid)?;
        let result = self.socket()?.write(bytes);
        self.progress(result, false)
    }
    fn flush(&mut self) -> Result<FlushProgress, Error> {
        self.socket()?;
        Ok(FlushProgress::Complete)
    }
    fn close(&mut self) -> Result<FlushProgress, Error> {
        self.socket()?;
        if !self.write_closed {
            if let Err(error) = self.socket()?.shutdown(Shutdown::Write) {
                return Err(self.fail(error.into()));
            }
            self.write_closed = true;
        }
        Ok(FlushProgress::Complete)
    }
    fn abort(&mut self) {
        self.fail(Error::Invalid);
    }
}
impl Drop for UnixTransport {
    fn drop(&mut self) {
        self.abort();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn readability_wait_preserves_the_first_byte_and_eof() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut wire = UnixTransport::from_stream(stream).unwrap();
        peer.write_all(b"ab").unwrap();
        peer.shutdown(Shutdown::Write).unwrap();
        let until = Instant::now() + Duration::from_secs(30);
        wire.wait_readable(until).unwrap();
        wire.wait_readable(until).unwrap();
        let mut bytes = [0; 2];
        assert_eq!(wire.read(&mut bytes).unwrap(), IoProgress::Bytes(1));
        assert_eq!(bytes.first(), Some(&b'a'));
        assert_eq!(wire.read(&mut bytes).unwrap(), IoProgress::Bytes(1));
        assert_eq!(bytes.first(), Some(&b'b'));
        wire.wait_readable(until).unwrap();
        assert_eq!(wire.read(&mut bytes).unwrap(), IoProgress::Closed);
    }
    #[test]
    fn an_empty_readiness_slice_does_not_retire_the_connection() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut wire = UnixTransport::from_stream(stream).unwrap();
        assert!(!wire.wait_readable_slice(Instant::now()).unwrap());
        peer.write_all(b"x").unwrap();
        wire.wait_readable(Instant::now() + Duration::from_secs(30))
            .unwrap();
        let mut byte = [0];
        assert_eq!(wire.read(&mut byte).unwrap(), IoProgress::Bytes(1));
        assert_eq!(byte, [b'x']);
    }
    #[test]
    fn expired_wait_retires_the_stream() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut wire = UnixTransport::from_stream(stream).unwrap();
        assert_eq!(wire.wait_readable(Instant::now()), Err(Error::Deadline));
        assert_eq!(wire.read(&mut [0]), Err(Error::Deadline));
        assert_eq!(wire.write(b"x"), Err(Error::Deadline));
    }
    #[test]
    fn half_close_preserves_inbound_bytes_and_writes_are_chunk_bounded() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut wire = UnixTransport::from_stream(stream).unwrap();
        let bytes = vec![1; SOCKET_CHUNK + 1];
        assert_eq!(
            wire.write_until(&bytes, Instant::now() + Duration::from_secs(30))
                .unwrap(),
            IoProgress::Bytes(SOCKET_CHUNK)
        );
        wire.close().unwrap();
        peer.write_all(b"x").unwrap();
        wire.wait_readable(Instant::now() + Duration::from_secs(30))
            .unwrap();
        let mut byte = [0];
        assert_eq!(wire.read(&mut byte).unwrap(), IoProgress::Bytes(1));
        assert_eq!(byte, [b'x']);
        assert!(wire.write(b"no").is_err());
    }
}
