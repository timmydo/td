//! Bounded shell-launch messages, pinned to a live sender after the greeting.
use crate::secret_sys;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

pub(crate) const LIMIT: usize = 40 * 1024;
const GREETING: &[u8; 8] = b"TDCS001\n";
const TIMEOUT: Duration = Duration::from_secs(2);

struct Peer {
    pidfd: File,
    credentials: secret_sys::Credentials,
    identity: (u64, u64),
}

pub(crate) struct Channel {
    stream: UnixStream,
    uid: u32,
    gid: u32,
    peer: Option<Peer>,
}

impl Channel {
    pub(crate) fn connect(mut stream: UnixStream, uid: u32, server: bool) -> io::Result<Self> {
        if secret_sys::peer_uid(&stream)? != if server { uid } else { 0 } {
            return Err(io::Error::other("wrong shell-launch connection owner"));
        }
        secret_sys::prepare_receiver(&stream)?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        // This byte only orders receiver-option installation. All authority
        // comes from the authenticated greeting and subsequent message bytes.
        if server {
            let mut ready = [0];
            stream.read_exact(&mut ready)?;
            if ready != [1] {
                return Err(io::Error::other("invalid shell-launch ready byte"));
            }
        } else {
            stream.write_all(&[1])?;
        }
        let mut channel = Self {
            stream,
            uid,
            gid: uid,
            peer: None,
        };
        let until = Instant::now() + TIMEOUT;
        if server {
            channel.write(GREETING, until)?;
        }
        let mut greeting = [0; 8];
        if channel.read(&mut greeting, until)?.is_some() || greeting != *GREETING {
            return Err(io::Error::other("invalid shell-launch greeting"));
        }
        if !server {
            channel.write(GREETING, until)?;
        }
        channel.live()?;
        Ok(channel)
    }

    pub(crate) fn live(&self) -> io::Result<()> {
        let peer = self
            .peer
            .as_ref()
            .ok_or_else(|| io::Error::other("missing shell-launch peer"))?;
        secret_sys::alive(peer.pidfd.as_fd())
    }

    fn sender(&mut self, sender: secret_sys::Sender) -> io::Result<Option<File>> {
        if sender.credentials.uid != self.uid || sender.credentials.gid != self.gid {
            return Err(io::Error::other("wrong shell-launch sender identity"));
        }
        secret_sys::alive(sender.pidfd.as_fd())?;
        let pidfd = File::from(sender.pidfd);
        let metadata = pidfd.metadata()?;
        let identity = (metadata.dev(), metadata.ino());
        if let Some(peer) = &self.peer {
            if identity != peer.identity || sender.credentials != peer.credentials {
                return Err(io::Error::other("shell-launch sender changed"));
            }
            self.live()?;
        } else {
            self.peer = Some(Peer {
                pidfd,
                identity,
                credentials: sender.credentials,
            });
        }
        Ok(sender.descriptor)
    }

    fn read(&mut self, mut bytes: &mut [u8], until: Instant) -> io::Result<Option<File>> {
        let mut descriptor = None;
        while !bytes.is_empty() {
            self.stream.set_read_timeout(Some(remaining(until)?))?;
            let (count, sender) = match secret_sys::receive(&self.stream, bytes) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if let Some(file) = self.sender(sender)? {
                if descriptor.replace(file).is_some() {
                    return Err(io::Error::other("multiple shell-launch descriptors"));
                }
            }
            bytes = bytes
                .get_mut(count..)
                .ok_or_else(|| io::Error::other("invalid shell-launch read"))?;
        }
        remaining(until)?;
        Ok(descriptor)
    }

    fn write(&mut self, mut bytes: &[u8], until: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            self.stream.set_write_timeout(Some(remaining(until)?))?;
            let count = match self.stream.write(bytes) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if count == 0 {
                return Err(io::Error::other("shell-launch write stopped"));
            }
            bytes = bytes
                .get(count..)
                .ok_or_else(|| io::Error::other("invalid shell-launch write"))?;
        }
        remaining(until)?;
        Ok(())
    }

    pub(crate) fn send(&mut self, message: &[u8]) -> io::Result<()> {
        self.live()?;
        if message.len() > LIMIT {
            return Err(io::Error::other("shell-launch message too large"));
        }
        let until = Instant::now() + TIMEOUT;
        self.write(&(message.len() as u32).to_be_bytes(), until)?;
        self.write(message, until)
    }

    pub(crate) fn receive(&mut self) -> io::Result<Vec<u8>> {
        self.live()?;
        let until = Instant::now() + TIMEOUT;
        let mut size = [0; 4];
        if self.read(&mut size, until)?.is_some() {
            return Err(io::Error::other("unsolicited shell-launch descriptor"));
        }
        let size = u32::from_be_bytes(size) as usize;
        if size > LIMIT {
            return Err(io::Error::other("shell-launch message too large"));
        }
        let mut message = vec![0; size];
        if self.read(&mut message, until)?.is_some() {
            return Err(io::Error::other("unsolicited shell-launch descriptor"));
        }
        self.live()?;
        Ok(message)
    }

    pub(crate) fn send_terminal(&mut self, master: &File) -> io::Result<()> {
        self.live()?;
        self.stream.set_write_timeout(Some(TIMEOUT))?;
        secret_sys::send_descriptor(&self.stream, &[1], master)
    }

    pub(crate) fn receive_terminal(&mut self) -> io::Result<File> {
        let mut reply = [0];
        let descriptor = self.read(&mut reply, Instant::now() + TIMEOUT)?;
        if reply != [1] {
            return Err(io::Error::other("Claude launch refused"));
        }
        descriptor.ok_or_else(|| io::Error::other("Claude launch omitted its terminal"))
    }

    pub(crate) fn idle(&mut self) -> io::Result<()> {
        self.live()?;
        self.stream.set_nonblocking(true)?;
        let mut byte = [0];
        let result = self.stream.read(&mut byte);
        self.stream.set_nonblocking(false)?;
        match result {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(()),
            _ => Err(io::Error::other(
                "shell-launch client disconnected or sent extra data",
            )),
        }
    }
}

fn remaining(until: Instant) -> io::Result<Duration> {
    until
        .checked_duration_since(Instant::now())
        .filter(|time| !time.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "shell-launch deadline expired"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub(crate) mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn prepared(stream: UnixStream) -> Channel {
        secret_sys::prepare_receiver(&stream).unwrap();
        let identity = std::fs::metadata("/proc/self").unwrap();
        // Host test accounts need not have matching numeric UID/GID.
        Channel {
            stream,
            uid: identity.uid(),
            gid: identity.gid(),
            peer: None,
        }
    }

    fn pin(receiver: &mut Channel) {
        let mut bytes = [0; 8];
        assert!(receiver
            .read(&mut bytes, Instant::now() + TIMEOUT)
            .unwrap()
            .is_none());
        assert_eq!(bytes, *GREETING);
        receiver.live().unwrap();
    }

    pub(crate) fn pair() -> (Channel, Channel) {
        let (left, right) = UnixStream::pair().unwrap();
        let (mut left, mut right) = (prepared(left), prepared(right));
        left.write(GREETING, Instant::now() + TIMEOUT).unwrap();
        pin(&mut right);
        right.write(GREETING, Instant::now() + TIMEOUT).unwrap();
        pin(&mut left);
        (left, right)
    }

    #[test]
    fn authenticated_frames_preserve_literal_bytes_and_allow_empty_messages() {
        let (mut left, mut right) = pair();
        for payload in [b"literal;argument\0\n".as_slice(), b"".as_slice()] {
            left.send(payload).unwrap();
            assert_eq!(right.receive().unwrap(), payload);
        }
        right.send(b"exit:7").unwrap();
        assert_eq!(left.receive().unwrap(), b"exit:7");
        right.idle().unwrap();
        left.stream.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(right.idle().is_err());
    }

    #[test]
    fn transferred_terminal_is_the_same_live_pty() {
        let (mut left, mut right) = pair();
        let (master, mut slave) = crate::terminal::terminal_pair().unwrap();
        left.send_terminal(&master).unwrap();
        let mut received = right.receive_terminal().unwrap();
        drop(master);
        crate::terminal_sys::relay_window_set(received.as_fd(), &[33, 101, 0, 0]).unwrap();
        assert_eq!(
            crate::terminal_sys::relay_window_get(slave.as_fd()).unwrap(),
            [33, 101, 0, 0]
        );
        received.write_all(b"through transferred master\n").unwrap();
        let mut input = [0; 27];
        slave.read_exact(&mut input).unwrap();
        assert_eq!(&input, b"through transferred master\n");
    }

    #[test]
    fn oversized_incoming_and_outgoing_frames_are_refused() {
        let (mut left, mut right) = pair();
        assert!(left
            .send(&vec![0; LIMIT + 1])
            .unwrap_err()
            .to_string()
            .contains("too large"));
        left.write(
            &((LIMIT + 1) as u32).to_be_bytes(),
            Instant::now() + TIMEOUT,
        )
        .unwrap();
        assert!(right
            .receive()
            .unwrap_err()
            .to_string()
            .contains("too large"));
    }

    #[test]
    fn ordinary_frames_refuse_rights_in_the_length_or_payload() {
        for in_header in [true, false] {
            let (mut left, mut right) = pair();
            let (master, _slave) = crate::terminal::terminal_pair().unwrap();
            if in_header {
                secret_sys::send_descriptor(&left.stream, &1_u32.to_be_bytes(), &master).unwrap();
            } else {
                left.write(&1_u32.to_be_bytes(), Instant::now() + TIMEOUT)
                    .unwrap();
                secret_sys::send_descriptor(&left.stream, &[9], &master).unwrap();
            }
            assert!(right
                .receive()
                .unwrap_err()
                .to_string()
                .contains("unsolicited"));
        }
    }

    #[test]
    fn terminal_response_requires_success_and_one_descriptor() {
        for byte in [0, 1] {
            let (mut left, mut right) = pair();
            left.write(&[byte], Instant::now() + TIMEOUT).unwrap();
            let error = right.receive_terminal().unwrap_err().to_string();
            assert!(error.contains(if byte == 0 { "refused" } else { "omitted" }));
        }
    }

    #[test]
    fn dead_sender_is_refused_even_with_a_buffered_frame() {
        let (receiver, sender) = UnixStream::pair().unwrap();
        let mut receiver = prepared(receiver);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "shell_channel::tests::sender_fixture",
                "--ignored",
            ])
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(sender)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        pin(&mut receiver);
        receiver.stream.write_all(&[1]).unwrap();
        assert!(child.wait().unwrap().success());
        assert!(receiver
            .receive()
            .unwrap_err()
            .to_string()
            .contains("no longer alive"));
    }

    #[test]
    #[ignore = "process fixture controlled by dead_sender_is_refused_even_with_a_buffered_frame"]
    fn sender_fixture() {
        let mut stream = UnixStream::from(io::stdin().as_fd().try_clone_to_owned().unwrap());
        stream.write_all(GREETING).unwrap();
        let mut release = [0];
        stream.read_exact(&mut release).unwrap();
        assert_eq!(release, [1]);
        stream.write_all(&1_u32.to_be_bytes()).unwrap();
        stream.write_all(b"x").unwrap();
    }
}
