//! A root-created stream with a live kernel-pinned sender on every receive.

use crate::sys;
use std::fs::File;
use std::io::{self, Write};
use std::net::Shutdown;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const GREETING: &[u8; 8] = b"TDAT001\n";
const TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_MESSAGE: usize = 4096;

struct Peer {
    pidfd: File,
    credentials: sys::Credentials,
    device: u64,
    inode: u64,
}

pub struct Channel {
    stream: UnixStream,
    expected_uid: u32,
    peer: Option<Peer>,
    closed: bool,
}

impl Channel {
    pub fn from_stdin(expected_uid: u32) -> io::Result<Self> {
        let stream = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned()?);
        Self::connect(stream, expected_uid, 0)
    }

    fn connect(stream: UnixStream, expected_uid: u32, creator_uid: u32) -> io::Result<Self> {
        let mut channel = Self {
            stream,
            expected_uid,
            peer: None,
            closed: false,
        };
        let result = (|| {
            if matches!(expected_uid, 65534 | u32::MAX) {
                return Err(io::Error::other("unsupported peer uid"));
            }
            if !channel.stream.local_addr()?.is_unnamed()
                || !channel.stream.peer_addr()?.is_unnamed()
            {
                return Err(io::Error::other(
                    "authority channel must be an unnamed socketpair",
                ));
            }
            let creator = sys::prepare(&channel.stream)?;
            if creator.uid != creator_uid {
                return Err(io::Error::other(
                    "authority channel has the wrong creator uid",
                ));
            }
            let deadline = deadline()?;
            channel.write(GREETING, deadline)?;
            let mut greeting = [0u8; 8];
            channel.read(&mut greeting, deadline)?;
            if greeting != *GREETING {
                return Err(io::Error::other("unsupported authority channel greeting"));
            }
            channel.live()
        })();
        channel.finish(result)?;
        Ok(channel)
    }

    pub fn send(&mut self, message: &[u8]) -> io::Result<()> {
        let result = (|| {
            self.live()?;
            if message.len() > MAX_MESSAGE {
                return Err(io::Error::other("authority message exceeds 4096 bytes"));
            }
            let deadline = deadline()?;
            self.write(&(message.len() as u32).to_be_bytes(), deadline)?;
            self.write(message, deadline)?;
            self.live()
        })();
        self.finish(result)
    }

    pub fn receive(&mut self) -> io::Result<Vec<u8>> {
        let result = (|| {
            self.live()?;
            let deadline = deadline()?;
            let mut header = [0u8; 4];
            self.read(&mut header, deadline)?;
            let length = u32::from_be_bytes(header) as usize;
            if length > MAX_MESSAGE {
                return Err(io::Error::other("authority message exceeds 4096 bytes"));
            }
            let mut message = vec![0u8; length];
            self.read(&mut message, deadline)?;
            self.live()?;
            Ok(message)
        })();
        self.finish(result)
    }

    fn finish<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if result.is_err() {
            self.closed = true;
            let _ = self.stream.shutdown(Shutdown::Both);
            self.peer = None;
        }
        result
    }

    fn live(&self) -> io::Result<()> {
        if self.closed {
            return Err(io::Error::other("authority channel is closed"));
        }
        let peer = self
            .peer
            .as_ref()
            .ok_or_else(|| io::Error::other("authority channel has no peer"))?;
        sys::alive(peer.pidfd.as_fd())
    }

    fn sender(&mut self, sender: sys::Sender) -> io::Result<()> {
        if sender.credentials.uid != self.expected_uid {
            return Err(io::Error::other(
                "authority channel has the wrong sender uid",
            ));
        }
        sys::alive(sender.pidfd.as_fd())?;
        let pidfd = File::from(sender.pidfd);
        let metadata = pidfd.metadata()?;
        if let Some(peer) = &self.peer {
            if sender.credentials != peer.credentials
                || metadata.dev() != peer.device
                || metadata.ino() != peer.inode
            {
                return Err(io::Error::other("authority channel sender changed"));
            }
            return self.live();
        }
        self.peer = Some(Peer {
            pidfd,
            credentials: sender.credentials,
            device: metadata.dev(),
            inode: metadata.ino(),
        });
        Ok(())
    }

    fn read(&mut self, mut bytes: &mut [u8], deadline: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            self.stream.set_read_timeout(Some(remaining(deadline)?))?;
            let (count, sender) = match sys::receive(&self.stream, bytes) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            self.sender(sender)?;
            bytes = bytes
                .get_mut(count..)
                .ok_or_else(|| io::Error::other("invalid channel read count"))?;
        }
        remaining(deadline)?;
        Ok(())
    }

    fn write(&mut self, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            self.stream.set_write_timeout(Some(remaining(deadline)?))?;
            let count = match self.stream.write(bytes) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "authority channel stopped writing",
                ));
            }
            bytes = bytes
                .get(count..)
                .ok_or_else(|| io::Error::other("invalid channel write count"))?;
        }
        remaining(deadline)?;
        Ok(())
    }
}

fn deadline() -> io::Result<Instant> {
    Instant::now()
        .checked_add(TIMEOUT)
        .ok_or_else(|| io::Error::other("authority deadline overflow"))
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|time| !time.is_zero())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "authority channel deadline expired",
            )
        })
}

impl Drop for Channel {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
#[path = "../tests/channel.rs"]
mod tests;
