//! Single-owner executor readiness shared with its nonblocking wire adapter.
use super::{Error, FlushProgress, IoProgress, Transport};
use crate::{
    ports::Error as PortError,
    readiness_sys::{self, Connected, Interest, Ready},
    transport::TcpTransport,
    unix_transport::UnixTransport,
};
use std::{
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::{Wake, Waker},
    time::Instant,
};

pub(super) trait DriverTransport: Transport + Send {
    fn descriptor(&self) -> Result<Connected<'_>, PortError>;
}
impl DriverTransport for TcpTransport {
    fn descriptor(&self) -> Result<Connected<'_>, PortError> {
        self.connected().map(Connected::Tcp)
    }
}
impl DriverTransport for UnixTransport {
    fn descriptor(&self) -> Result<Connected<'_>, PortError> {
        self.connected().map(Connected::Unix)
    }
}

struct Signal {
    waiting: AtomicBool,
    active: AtomicBool,
    sender: UnixStream,
}
impl Wake for Signal {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        if self.active.swap(true, Ordering::SeqCst) || !self.waiting.load(Ordering::SeqCst) {
            return;
        }
        // A full wake socket already has a readable notification. A failed
        // sender wakes its owner through EOF instead of losing a notification.
        match (&self.sender).write(&[1]) {
            Ok(1) => (),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
            _ => {
                let _ = self.sender.shutdown(Shutdown::Write);
            }
        }
    }
}

struct Socket {
    wire: Box<dyn DriverTransport>,
    interest: Interest,
    deadline: Instant,
}
pub(super) struct Driver {
    socket: Mutex<Socket>,
    signal: Arc<Signal>,
    receiver: UnixStream,
}
impl Driver {
    pub(super) fn new(
        wire: Box<dyn DriverTransport>,
        deadline: Instant,
    ) -> Result<Arc<Self>, Error> {
        let (sender, receiver) = UnixStream::pair().map_err(|_| Error::Transport)?;
        sender.set_nonblocking(true).map_err(|_| Error::Transport)?;
        receiver
            .set_nonblocking(true)
            .map_err(|_| Error::Transport)?;
        Ok(Arc::new(Self {
            socket: Mutex::new(Socket {
                wire,
                interest: Interest::default(),
                deadline,
            }),
            receiver,
            signal: Arc::new(Signal {
                waiting: AtomicBool::new(false),
                active: AtomicBool::new(false),
                sender,
            }),
        }))
    }
    pub(super) fn waker(&self) -> Waker {
        Waker::from(Arc::clone(&self.signal))
    }
    pub(super) fn begin_poll(&self, deadline: Instant) -> Result<(), Error> {
        let mut socket = self.socket.lock().map_err(|_| Error::Transport)?;
        socket.deadline = deadline;
        socket.interest = Interest::default();
        self.signal.active.store(false, Ordering::Release);
        Ok(())
    }
    pub(super) fn active(&self) -> bool {
        self.signal.active.load(Ordering::Acquire)
    }
    pub(super) fn wait(&self, deadline: Instant) -> Result<bool, Error> {
        self.signal.waiting.store(true, Ordering::SeqCst);
        let result = if self.signal.active.load(Ordering::SeqCst) {
            Ok(true)
        } else {
            self.wait_socket(deadline)
        };
        self.signal.waiting.store(false, Ordering::SeqCst);
        result
    }
    fn wait_socket(&self, deadline: Instant) -> Result<bool, Error> {
        let socket = self.socket.lock().map_err(|_| Error::Transport)?;
        let ready = readiness_sys::wait(
            socket.wire.descriptor().map_err(|_| Error::Transport)?,
            socket.interest,
            Some(&self.receiver),
            deadline,
        )
        .map_err(|_| Error::Transport)?;
        match ready {
            Ready::Socket => Ok(true),
            Ready::Timeout => Ok(false),
            Ready::Wake => match (&self.receiver).read(&mut [0; 64]) {
                Ok(0) => Err(Error::Transport),
                Ok(_) => Ok(true),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) =>
                {
                    Ok(true)
                }
                Err(_) => Err(Error::Transport),
            },
        }
    }
    pub(super) fn wait_until(&self, deadline: Instant) -> Result<(), Error> {
        if self.wait(deadline)? {
            Ok(())
        } else {
            Err(Error::Deadline)
        }
    }
}

pub(super) struct Network(pub(super) Arc<Driver>);
impl Transport for Network {
    fn read(&mut self, output: &mut [u8]) -> Result<IoProgress, PortError> {
        let mut socket = self.0.socket.lock().map_err(|_| PortError::Invalid)?;
        let result = socket.wire.read(output)?;
        if result == IoProgress::Pending {
            socket.interest.read = true;
        } else if matches!(result, IoProgress::Bytes(n) if n != 0) {
            self.0.signal.active.store(true, Ordering::Release);
        }
        Ok(result)
    }
    fn write(&mut self, input: &[u8]) -> Result<IoProgress, PortError> {
        let mut socket = self.0.socket.lock().map_err(|_| PortError::Invalid)?;
        if Instant::now() >= socket.deadline {
            return Err(PortError::Deadline);
        }
        let result = socket.wire.write(input)?;
        if result == IoProgress::Pending {
            socket.interest.write = true;
        } else if matches!(result, IoProgress::Bytes(n) if n != 0) {
            self.0.signal.active.store(true, Ordering::Release);
        }
        Ok(result)
    }
    fn flush(&mut self) -> Result<FlushProgress, PortError> {
        self.0
            .socket
            .lock()
            .map_err(|_| PortError::Invalid)?
            .wire
            .flush()
    }
    fn close(&mut self) -> Result<FlushProgress, PortError> {
        self.0
            .socket
            .lock()
            .map_err(|_| PortError::Invalid)?
            .wire
            .close()
    }
    fn abort(&mut self) {
        if let Ok(mut socket) = self.0.socket.lock() {
            socket.wire.abort();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{thread, time::Duration};

    #[test]
    fn an_external_waker_releases_an_idle_driver_without_readable_database_bytes() {
        let (raw, _peer) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let driver =
            Driver::new(Box::new(UnixTransport::from_stream(raw).unwrap()), deadline).unwrap();
        let mut wire = Network(Arc::clone(&driver));
        assert_eq!(wire.read(&mut [0]).unwrap(), IoProgress::Pending);
        thread::scope(|scope| {
            scope.spawn(|| {
                while !driver.signal.waiting.load(Ordering::SeqCst) {
                    assert!(Instant::now() < deadline);
                    thread::yield_now();
                }
                let waker = driver.waker();
                for _ in 0..128 {
                    waker.wake_by_ref();
                }
            });
            assert!(driver.wait(deadline).unwrap());
        });
        driver.begin_poll(deadline).unwrap();
        assert_eq!(wire.read(&mut [0]).unwrap(), IoProgress::Pending);
        // A wake racing with the first return can leave one harmless token.
        let timeout = Instant::now() + Duration::from_millis(15);
        if driver.wait(timeout).unwrap() {
            assert!(!driver.wait(timeout).unwrap());
        }
    }
}
