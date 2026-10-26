//! Socket deadlines refreshed before every raw HTTPS read and write.
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    sync::{Arc, Mutex},
    time::Instant,
};

#[derive(Clone, Debug)]
pub(super) struct Deadline(Arc<Mutex<Instant>>);
impl Deadline {
    pub fn new(end: Instant) -> Self {
        Self(Arc::new(Mutex::new(end)))
    }
    pub fn reset(&self, end: Instant) -> io::Result<()> {
        *self
            .0
            .lock()
            .map_err(|_| io::Error::other("HTTPS deadline lock unavailable"))? = end;
        Ok(())
    }
    fn remaining(&self, now: Instant) -> io::Result<std::time::Duration> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("HTTPS deadline lock unavailable"))?
            .checked_duration_since(now)
            .filter(|value| !value.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "HTTPS exchange deadline"))
    }
}

pub(super) struct Connector {
    pub config: Arc<ureq::rustls::ClientConfig>,
    pub deadline: Deadline,
}
impl ureq::TlsConnector for Connector {
    fn connect(
        &self,
        name: &str,
        stream: Box<dyn ureq::ReadWrite>,
    ) -> Result<Box<dyn ureq::ReadWrite>, ureq::Error> {
        ureq::TlsConnector::connect(
            &self.config,
            name,
            Box::new(Socket {
                inner: stream,
                deadline: self.deadline.clone(),
            }),
        )
    }
}
#[derive(Debug)]
struct Socket {
    inner: Box<dyn ureq::ReadWrite>,
    deadline: Deadline,
}
impl Socket {
    fn prepare(&self) -> io::Result<()> {
        let remaining = self.deadline.remaining(Instant::now())?;
        let socket = self
            .inner
            .socket()
            .ok_or_else(|| io::Error::other("HTTPS transport has no bounded socket"))?;
        socket.set_read_timeout(Some(remaining))?;
        socket.set_write_timeout(Some(remaining))
    }
}
impl Read for Socket {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.prepare()?;
        self.inner.read(bytes)
    }
}
impl Write for Socket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.prepare()?;
        self.inner.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.prepare()?;
        self.inner.flush()
    }
}
impl ureq::ReadWrite for Socket {
    fn socket(&self) -> Option<&TcpStream> {
        self.inner.socket()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{net::TcpListener, time::Duration};
    use ureq::ReadWrite;

    #[test]
    fn reused_socket_restores_deadlines_and_rejects_expired_work_before_write() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let now = Instant::now();
        let deadline = Deadline::new(now);
        let mut stream = Socket {
            inner: Box::new(client),
            deadline: deadline.clone(),
        };
        assert_eq!(
            stream.write(b"discard").unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        deadline.reset(now + Duration::from_secs(60)).unwrap();
        let socket = stream.socket().unwrap();
        socket.set_read_timeout(None).unwrap();
        socket.set_write_timeout(None).unwrap();
        stream.write_all(b"x").unwrap();
        assert!(stream.socket().unwrap().read_timeout().unwrap().is_some());
        assert!(stream.socket().unwrap().write_timeout().unwrap().is_some());
        let mut byte = [0];
        peer.read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"x");
        deadline.reset(now).unwrap();
        assert_eq!(stream.flush().unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}
