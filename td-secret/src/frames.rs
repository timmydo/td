//! The blocking framed endpoint a private root worker holds to td-authd,
//! and the checks its startup makes: td-secret's token and login workers'
//! and td-install's storage worker's (td-secret/DESIGN.md, "Login-key
//! worker"; td-install/DESIGN.md, "Storage operation worker"). Pure `std`
//! over the socket alone: each crate reads its own process files, under
//! its own path rules, and hands the results in.

use std::ffi::OsString;
use std::fs::Metadata;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const LIMIT: usize = 289;
pub(crate) const FRAME_TIME: Duration = Duration::from_secs(5);
/// Root's PIN frame, sent after a presented secret step: this tag, then a
/// PIN's 4 to 63 bytes or a recovery key's 48 digits.
pub(crate) const SECRET: u8 = 0x16;
/// The longest secret frame: its tag and 63 bytes.
const SECRET_FRAME: usize = 64;

pub(crate) struct Wire {
    stream: UnixStream,
    deadline: Instant,
    /// The open token session's deadline, which bounds every wait while it
    /// is open without narrowing the operation's.
    session: Option<Instant>,
    /// A frame or the operation ran out of time; nothing else sets it.
    expired: bool,
}

impl Wire {
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Whether a failure so far was a frame or operation deadline.
    pub(crate) fn expired(&self) -> bool {
        self.expired
    }

    /// Narrows the operation deadline once the operation is known; never extends it.
    pub(crate) fn limit(&mut self, deadline: Instant) {
        self.deadline = self.deadline.min(deadline);
    }

    /// Bounds every wait by a token session's deadline while it is open;
    /// none once it has ended.
    pub(crate) fn session(&mut self, deadline: Option<Instant>) {
        self.session = deadline;
    }

    /// What the next wait may last until: the operation deadline, or an
    /// open session's if that is earlier.
    pub(crate) fn bound(&self) -> Instant {
        self.session
            .map_or(self.deadline, |session| session.min(self.deadline))
    }

    pub(crate) fn new(stream: UnixStream, deadline: Instant) -> Result<Self, String> {
        stream
            .set_nonblocking(false)
            .map_err(|_| "set blocking operation endpoint")?;
        Ok(Self {
            stream,
            deadline,
            session: None,
            expired: false,
        })
    }

    fn frame_remaining(&mut self, deadline: Instant) -> Result<Duration, String> {
        let result = remaining(self.bound()).and_then(|_| {
            remaining(deadline).map_err(|_| "private operation frame timed out".into())
        });
        self.expired |= result.is_err();
        result
    }

    pub(crate) fn frame_deadline(&self) -> Result<Instant, String> {
        Instant::now()
            .checked_add(FRAME_TIME)
            .map(|deadline| deadline.min(self.bound()))
            .ok_or_else(|| "operation deadline overflow".into())
    }

    pub(crate) fn read(&mut self, mut bytes: &mut [u8], deadline: Instant) -> Result<(), String> {
        while !bytes.is_empty() {
            let timeout = self.frame_remaining(deadline)?;
            self.stream
                .set_read_timeout(Some(timeout))
                .map_err(|_| "set operation read timeout")?;
            let count = match self.stream.read(bytes) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    self.frame_remaining(deadline)?;
                    self.expired = true;
                    return Err("private operation frame timed out".into());
                }
                result => result.map_err(|_| "read private operation frame")?,
            };
            if count == 0 {
                return Err("operation authority disconnected".into());
            }
            bytes = bytes
                .get_mut(count..)
                .ok_or("invalid operation read count")?;
        }
        self.frame_remaining(deadline).map(|_| ())
    }

    pub(crate) fn receive(&mut self) -> Result<Vec<u8>, String> {
        let mut frame = self.receive_cleared()?;
        Ok(std::mem::take(&mut frame.0).into_vec())
    }

    /// One frame in clearing storage: a misordered PIN, or a partial
    /// frame, is zeroed when it is dropped.
    pub(crate) fn receive_cleared(&mut self) -> Result<Cleared, String> {
        self.receive_cleared_by(None)
    }

    /// `receive_cleared`, whose header waits until `header` when given,
    /// the rest then having one frame time.
    fn receive_cleared_by(&mut self, header: Option<Instant>) -> Result<Cleared, String> {
        let mut deadline = self.frame_deadline()?;
        let mut length = [0; 2];
        if let Some(header) = header {
            self.read(&mut length, header)?;
            deadline = self.frame_deadline()?;
        } else {
            self.read(&mut length, deadline)?;
        }
        let length = usize::from(u16::from_be_bytes(length));
        if length == 0 || length > LIMIT {
            return Err("invalid operation frame length".into());
        }
        let mut frame = Cleared(vec![0; length].into_boxed_slice());
        self.read(&mut frame.0, deadline)?;
        Ok(frame)
    }

    /// One secret frame. A person types it, so its header waits for the
    /// operation deadline, or an open session's, rather than a frame's; the
    /// rest then has one frame time. The bytes land in clearing storage and
    /// never enter a diagnostic; the caller admits them under its own
    /// profile.
    pub(crate) fn receive_secret(&mut self) -> Result<Cleared, String> {
        let mut header = [0; 2];
        self.read(&mut header, self.bound())?;
        let length = usize::from(u16::from_be_bytes(header));
        if !(2..=SECRET_FRAME).contains(&length) {
            return Err("invalid operation PIN frame length".into());
        }
        let deadline = self.frame_deadline()?;
        let mut tag = [0];
        self.read(&mut tag, deadline)?;
        if tag != [SECRET] {
            return Err("operation frame is not a PIN".into());
        }
        let mut secret = Cleared(vec![0; length - 1].into_boxed_slice());
        self.read(&mut secret.0, deadline)?;
        Ok(secret)
    }

    pub(crate) fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.is_empty() || bytes.len() > LIMIT {
            return Err("invalid operation frame length".into());
        }
        let deadline = self.frame_deadline()?;
        let mut frame = Vec::with_capacity(bytes.len() + 2);
        frame.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        frame.extend_from_slice(bytes);
        let mut pending = frame.as_slice();
        while !pending.is_empty() {
            let timeout = self.frame_remaining(deadline)?;
            self.stream
                .set_write_timeout(Some(timeout))
                .map_err(|_| "set operation write timeout")?;
            let count = match self.stream.write(pending) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    self.frame_remaining(deadline)?;
                    self.expired = true;
                    return Err("private operation frame timed out".into());
                }
                result => result.map_err(|_| "write private operation frame")?,
            };
            if count == 0 {
                return Err("operation authority stopped reading".into());
            }
            pending = pending
                .get(count..)
                .ok_or("invalid operation write count")?;
        }
        self.frame_remaining(deadline).map(|_| ())
    }

    /// One round: `tag`, the caller's 32 random `round` bytes and the
    /// encoded `description`, which root must answer with `answer` and the
    /// same bytes. The answer's header waits until `wait` when given, as a
    /// typed approval key's does.
    pub(crate) fn exchange(
        &mut self,
        tag: u8,
        answer: u8,
        round: [u8; 32],
        description: &[u8],
        wait: Option<Instant>,
    ) -> Result<(), String> {
        let mut message = vec![tag];
        message.extend_from_slice(&round);
        message.extend_from_slice(description);
        self.send(&message)?;
        let reply = self.receive_cleared_by(wait)?;
        let reply = reply.bytes();
        if reply.first() != Some(&answer) || reply.get(1..) != message.get(1..) {
            return Err("operation acknowledgement does not match its request and round".into());
        }
        Ok(())
    }
}

/// Zeroed on drop, including after a partial read.
pub(crate) struct Cleared(pub(crate) Box<[u8]>);
impl Cleared {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }
}
impl Drop for Cleared {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}

pub(crate) fn remaining(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| "private token operation expired".into())
}

/// What a worker's startup read about itself, through its own crate's
/// file rules: `/proc/self/status`, read to at most 8193 bytes; the
/// metadata of `/proc/self/fd/0`, `1` and `2`; and the names under
/// `/proc/self/fd`, at most five, read by a listing that observes itself
/// as descriptor 3.
pub(crate) struct Process {
    pub(crate) status: String,
    pub(crate) descriptors: [Metadata; 3],
    pub(crate) open: Vec<OsString>,
}

/// The private endpoint, once `process` shows a single thread, a
/// root-owned descriptor 0 no log descriptor aliases, only descriptors 0
/// to 2 inherited, every user ID root, and an unnamed socketpair on
/// standard input.
pub(crate) fn endpoint(process: Process) -> Result<UnixStream, String> {
    let uids = process
        .status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .map(|ids| ids.split_whitespace().collect::<Vec<_>>());
    if uids.as_deref() != Some(&["0", "0", "0", "0"][..]) {
        return Err("token operation requires every user ID to be root".into());
    }
    if process.status.len() > 8192
        || !process.status.lines().any(|line| {
            line.strip_prefix("Threads:")
                .is_some_and(|value| value.trim() == "1")
        })
    {
        return Err("token operation requires single-threaded startup".into());
    }
    let [input, output, error] = &process.descriptors;
    if input.uid() != 0 {
        return Err("operation endpoint must be root owned".into());
    }
    for output in [output, error] {
        if (input.dev(), input.ino()) == (output.dev(), output.ino()) {
            return Err("operation log aliases its private endpoint".into());
        }
    }
    let mut open = process.open;
    open.sort();
    if open != ["0", "1", "2", "3"].map(OsString::from) {
        return Err("token operation inherited extra descriptors".into());
    }
    let stream = UnixStream::from(
        std::io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .map_err(|_| "retain private operation endpoint")?,
    );
    if !stream
        .local_addr()
        .map_err(|_| "inspect operation endpoint")?
        .is_unnamed()
        || !stream
            .peer_addr()
            .map_err(|_| "inspect operation peer")?
            .is_unnamed()
    {
        return Err("token operation requires an unnamed socketpair".into());
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Within `endpoint`, the status refusals come before its descriptor
    /// checks, the user IDs first: td-install's worker, which has no
    /// `require_root`, relies on this one. (The descriptor rules need a root-owned file, which a
    /// build sandbox has none of.)
    #[test]
    fn the_endpoint_refuses_before_its_socket() {
        let metadata = |path: &str| std::fs::metadata(path).unwrap();
        let process = |status: &str| Process {
            status: status.into(),
            descriptors: [metadata("/"), metadata("/proc"), metadata("/dev")],
            open: ["0", "1", "2", "3"].iter().map(OsString::from).collect(),
        };
        for (status, refusal) in [
            (
                "Uid:\t1000\t0\t0\t0\nThreads:\t1\n",
                "token operation requires every user ID to be root",
            ),
            (
                "Uid:\t0\t0\t0\nThreads:\t1\n",
                "token operation requires every user ID to be root",
            ),
            (
                "Threads:\t1\n",
                "token operation requires every user ID to be root",
            ),
            (
                "Uid:\t0\t0\t0\t0\nThreads:\t2\n",
                "token operation requires single-threaded startup",
            ),
        ] {
            assert_eq!(endpoint(process(status)).unwrap_err(), refusal);
        }
    }

    fn pair() -> (Wire, Wire) {
        let (one, two) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        (
            Wire::new(one, deadline).unwrap(),
            Wire::new(two, deadline).unwrap(),
        )
    }

    #[test]
    fn frame_timeout_diagnostic_distinguishes_operation_expiration() {
        let (mut child, _parent) = pair();
        assert_eq!(
            child.frame_remaining(Instant::now()).unwrap_err(),
            "private operation frame timed out"
        );
        child.deadline = Instant::now();
        assert_eq!(
            child.frame_remaining(Instant::now()).unwrap_err(),
            "private token operation expired"
        );
    }

    #[test]
    fn socket_timeout_preserves_the_overall_expiration_diagnostic() {
        let (mut child, _parent) = pair();
        child.deadline = Instant::now() + Duration::from_millis(30);
        assert_eq!(
            child.receive().unwrap_err(),
            "private token operation expired"
        );
    }

    #[test]
    fn frame_bounds_and_expiration_refuse_before_processing() {
        let (mut child, mut parent) = pair();
        parent.stream.write_all(&290u16.to_be_bytes()).unwrap();
        assert!(child.receive().is_err());
        let (mut child, mut parent) = pair();
        parent.stream.write_all(&0u16.to_be_bytes()).unwrap();
        assert!(child.receive().is_err());
        let (mut child, _parent) = pair();
        child.deadline = Instant::now();
        assert!(child.receive().is_err());
        assert!(child.send(&[1]).is_err());
        let (mut child, _parent) = pair();
        assert!(child.send(&vec![1; LIMIT + 1]).is_err());
        let (mut child, _parent) = pair();
        assert!(child.send(&[]).is_err());
    }
}
