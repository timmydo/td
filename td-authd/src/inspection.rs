//! Bounded read-only store inspection after root paired-session admission.

use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const LIFETIME: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum State {
    File,
    Platform,
    TokenUnrecoverable,
    TokenRecovery,
}

impl State {
    pub fn tag(self) -> u8 {
        match self {
            Self::File => 0,
            Self::Platform => 1,
            Self::TokenUnrecoverable => 2,
            Self::TokenRecovery => 3,
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        match bytes {
            [0x17, 0] => Ok(Self::File),
            [0x17, 1] => Ok(Self::Platform),
            [0x17, 2] => Ok(Self::TokenUnrecoverable),
            [0x17, 3] => Ok(Self::TokenRecovery),
            _ => Err("invalid store inspection result".into()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Event {
    Waiting,
    State(State),
    Unavailable,
}

pub(crate) struct Inspection {
    child: Option<Child>,
    wire: Option<UnixStream>,
    bytes: Vec<u8>,
    eof: bool,
    deadline: Instant,
    failed: bool,
    terminal: Option<Event>,
}

impl Inspection {
    /// Construct only after root launch admission and session preparation.
    pub fn start(owner: u32) -> Result<Self, String> {
        if owner != 1000 {
            return Err("unsupported store inspection owner".into());
        }
        let mut command = Command::new("/bin/td-secret");
        command.args(["inspect-store", "--uid", &owner.to_string()]);
        Self::spawn(command)
    }

    fn spawn(mut command: Command) -> Result<Self, String> {
        let deadline = Instant::now()
            .checked_add(LIFETIME)
            .ok_or("store inspection deadline overflow")?;
        let (parent, output) = UnixStream::pair().map_err(|e| e.to_string())?;
        parent.set_nonblocking(true).map_err(|e| e.to_string())?;
        let child = command
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::from(OwnedFd::from(output)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("start store inspection: {e}"))?;
        Ok(Self {
            child: Some(child),
            wire: Some(parent),
            bytes: Vec::with_capacity(3),
            eof: false,
            deadline,
            failed: false,
            terminal: None,
        })
    }

    fn receive(&mut self) -> Result<(), String> {
        if self.eof {
            return Ok(());
        }
        let wire = self.wire.as_mut().ok_or("missing inspection endpoint")?;
        // One read per tick, including interruptions; at most three bytes total.
        let mut buffer = [0; 3];
        let remaining = 3usize
            .checked_sub(self.bytes.len())
            .ok_or("inspection overflow")?;
        match wire.read(buffer.get_mut(..remaining).ok_or("inspection overflow")?) {
            Ok(0) => self.eof = true,
            Ok(count) => self
                .bytes
                .extend_from_slice(buffer.get(..count).ok_or("invalid read")?),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(_) => return Err("read store inspection failed".into()),
        }
        if self.bytes.len() > 2 {
            return Err("oversized store inspection".into());
        }
        Ok(())
    }

    pub fn poll(&mut self) -> Result<Event, String> {
        if let Some(event) = self.terminal {
            return Ok(event);
        }
        if !self.failed && (Instant::now() >= self.deadline || self.receive().is_err()) {
            self.failed = true;
            self.wire = None;
            if let Some(child) = &mut self.child {
                child
                    .kill()
                    .map_err(|e| format!("stop store inspection: {e}"))?;
            }
        }
        if let Some(child) = &mut self.child {
            let Some(status) = child
                .try_wait()
                .map_err(|e| format!("reap store inspection: {e}"))?
            else {
                return Ok(Event::Waiting);
            };
            self.child = None;
            if !status.success() || Instant::now() >= self.deadline {
                self.failed = true;
            }
        }
        // Even successful exit is insufficient until bounded output and EOF
        // are observed. A retained descendant endpoint cannot stall the caller.
        if !self.failed && !self.eof {
            return Ok(Event::Waiting);
        }
        if Instant::now() >= self.deadline {
            self.failed = true;
        }
        let event = if self.failed {
            Event::Unavailable
        } else {
            match State::decode(&self.bytes) {
                Ok(state) => Event::State(state),
                Err(_) => Event::Unavailable,
            }
        };
        self.wire = None;
        self.terminal = Some(event);
        Ok(event)
    }
}

impl Drop for Inspection {
    fn drop(&mut self) {
        self.wire = None;
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
#[path = "../tests/inspection.rs"]
pub(crate) mod tests;
