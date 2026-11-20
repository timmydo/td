//! Admitted Unix stream I/O. Endpoint authority and dialing live above this layer.
use crate::{
    ports::{Error, FlushProgress, IoProgress, Transport},
    transport::SOCKET_CHUNK,
};
use std::{
    io::{self, ErrorKind, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
};

pub struct UnixTransport {
    socket: Option<UnixStream>,
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
    pub(crate) fn connected(&self) -> Result<&UnixStream, Error> {
        self.socket
            .as_ref()
            .ok_or(self.failure.unwrap_or(Error::Invalid))
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
    #[test]
    fn half_close_preserves_inbound_bytes_and_writes_are_chunk_bounded() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut wire = UnixTransport::from_stream(stream).unwrap();
        let bytes = vec![1; SOCKET_CHUNK + 1];
        assert_eq!(wire.write(&bytes).unwrap(), IoProgress::Bytes(SOCKET_CHUNK));
        wire.close().unwrap();
        peer.write_all(b"x").unwrap();
        let mut byte = [0];
        assert_eq!(wire.read(&mut byte).unwrap(), IoProgress::Bytes(1));
        assert_eq!(byte, [b'x']);
        assert!(wire.write(b"no").is_err());
    }
}
