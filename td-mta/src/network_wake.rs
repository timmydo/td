//! A private, nonblocking wake socket for one network owner.
use crate::ports::Error;
use std::{
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::Mutex,
};

#[derive(Default)]
pub(crate) struct Notifications(Mutex<Option<UnixStream>>);

impl Notifications {
    pub(crate) fn attach(&self) -> Result<Receiver<'_>, Error> {
        let mut sender = self.0.lock().map_err(|_| Error::WriterStopped)?;
        if sender.is_some() {
            return Err(Error::Busy);
        }
        let (tx, socket) = UnixStream::pair()?;
        tx.set_nonblocking(true)?;
        socket.set_nonblocking(true)?;
        *sender = Some(tx);
        Ok(Receiver {
            owner: self,
            socket,
        })
    }
    pub(crate) fn notify(&self) {
        let sender = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut tx) = sender.as_ref() {
            // A full nonblocking socket already contains a notification.
            loop {
                match tx.write(&[1]) {
                    Ok(1) => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    _ => {
                        let _ = tx.shutdown(Shutdown::Write);
                        break;
                    }
                }
            }
        }
    }
}
pub(crate) struct Receiver<'a> {
    owner: &'a Notifications,
    pub(crate) socket: UnixStream,
}
impl Drop for Receiver<'_> {
    fn drop(&mut self) {
        self.owner
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
}
impl Receiver<'_> {
    pub(crate) fn consume(&self) -> Result<(), Error> {
        match (&self.socket).read(&mut [0; 256]) {
            Ok(0) => Err(Error::WriterStopped),
            Ok(_) => Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }
}
