//! Bounded read-only inspection after root paired-session admission: the
//! application store's state (request `17`), and the login record's for
//! request `1a` (td-authd/DESIGN.md, login-state amendment 1).

use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const LIFETIME: Duration = Duration::from_secs(2);
/// The store helper's result: `17` and its state.
const STORE_RESULT: usize = 2;
/// The login helper's longest result, td-secret's
/// `login_store::MAX_INSPECTION`: `1a 01`, the version, the count and
/// eight fingerprints.
const LOGIN_RESULT: usize = 36;
/// How often a synchronous wait looks at the helper again.
const WAIT_STEP: Duration = Duration::from_millis(2);

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
    /// The most result bytes this helper may write.
    limit: usize,
    eof: bool,
    deadline: Instant,
    failed: bool,
    /// The deadline, not the helper, ended the run.
    missed: bool,
    terminal: Option<Event>,
    /// Sends SIGKILL; a test's stands for one that has not taken effect,
    /// as for a child in uninterruptible sleep.
    stop: fn(&mut Child) -> io::Result<()>,
}

impl Inspection {
    /// Construct only after root launch admission and session preparation.
    pub fn start(owner: u32) -> Result<Self, String> {
        if owner != 1000 {
            return Err("unsupported store inspection owner".into());
        }
        let mut command = Command::new("/bin/td-secret");
        command.args(["inspect-store", "--uid", &owner.to_string()]);
        Self::spawn(command, STORE_RESULT)
    }

    /// The login record's read-only helper, which request `1a` runs only
    /// when the record's name exists and reads with `wait`.
    pub fn login(owner: u32) -> Result<Self, String> {
        if owner != 1000 {
            return Err("unsupported login inspection owner".into());
        }
        let mut command = Command::new("/bin/td-secret");
        command.args(["inspect-login", "--uid", &owner.to_string()]);
        Self::spawn(command, LOGIN_RESULT)
    }

    fn spawn(mut command: Command, limit: usize) -> Result<Self, String> {
        let deadline = Instant::now()
            .checked_add(LIFETIME)
            .ok_or("inspection deadline overflow")?;
        let (parent, output) = UnixStream::pair().map_err(|e| e.to_string())?;
        parent.set_nonblocking(true).map_err(|e| e.to_string())?;
        let child = command
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::from(OwnedFd::from(output)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("start inspection: {e}"))?;
        Ok(Self {
            child: Some(child),
            wire: Some(parent),
            bytes: Vec::with_capacity(limit.saturating_add(1)),
            limit,
            eof: false,
            deadline,
            failed: false,
            missed: false,
            terminal: None,
            stop: Child::kill,
        })
    }

    fn kill(&mut self) -> io::Result<()> {
        match &mut self.child {
            Some(child) => (self.stop)(child),
            None => Ok(()),
        }
    }

    fn receive(&mut self) -> Result<(), String> {
        if self.eof {
            return Ok(());
        }
        let wire = self.wire.as_mut().ok_or("missing inspection endpoint")?;
        // One read per tick, including interruptions; at most one byte past
        // the limit in total, which refuses.
        let mut buffer = [0; LOGIN_RESULT + 1];
        let remaining = self
            .limit
            .checked_add(1)
            .and_then(|bound| bound.checked_sub(self.bytes.len()))
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
            Err(_) => return Err("read inspection failed".into()),
        }
        if self.bytes.len() > self.limit {
            return Err("oversized inspection".into());
        }
        Ok(())
    }

    /// The helper's whole result once its run is over: `None` while it
    /// runs; then its bytes, only after EOF and a successful exit, each
    /// observed before the deadline, or `Some(None)` for anything else.
    fn result(&mut self) -> Result<Option<Option<&[u8]>>, String> {
        if !self.failed {
            let expired = Instant::now() >= self.deadline;
            if expired || self.receive().is_err() {
                self.failed = true;
                self.missed = expired;
                self.wire = None;
                self.kill().map_err(|e| format!("stop inspection: {e}"))?;
            }
        }
        if let Some(child) = &mut self.child {
            let Some(status) = child
                .try_wait()
                .map_err(|e| format!("reap inspection: {e}"))?
            else {
                return Ok(None);
            };
            self.child = None;
            if !status.success() {
                self.failed = true;
            }
        }
        // Even successful exit is insufficient until bounded output and EOF
        // are observed. A retained descendant endpoint cannot stall the caller.
        if !self.failed && !self.eof {
            return Ok(None);
        }
        if Instant::now() >= self.deadline && !self.failed {
            self.failed = true;
            self.missed = true;
        }
        self.wire = None;
        Ok(Some((!self.failed).then_some(self.bytes.as_slice())))
    }

    pub fn poll(&mut self) -> Result<Event, String> {
        if let Some(event) = self.terminal {
            return Ok(event);
        }
        let Some(result) = self.result()? else {
            return Ok(Event::Waiting);
        };
        let event = match result.map(State::decode) {
            Some(Ok(state)) => Event::State(state),
            _ => Event::Unavailable,
        };
        self.terminal = Some(event);
        Ok(event)
    }

    /// Waits for the helper synchronously, never past its deadline: its
    /// bytes, or `None` when it failed in any way. A helper the deadline or
    /// a failure ended is killed and answered at once, unreaped: its owner
    /// keeps it and collects it with `reaped`, never waiting for the kernel.
    pub fn wait(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.result() {
                Ok(Some(result)) => return result.map(<[u8]>::to_vec),
                Ok(None) => {}
                Err(_) => {
                    self.failed = true;
                    self.wire = None;
                    return None;
                }
            }
            if self.failed {
                return None;
            }
            std::thread::sleep(WAIT_STEP);
        }
    }

    /// Whether the deadline ended the run.
    pub fn missed(&self) -> bool {
        self.missed
    }

    /// Whether the helper is gone, reaping it if it has exited; it never
    /// blocks, and kills again a helper still running.
    pub fn reaped(&mut self) -> bool {
        let Some(child) = &mut self.child else {
            return true;
        };
        match child.try_wait() {
            Ok(None) => {
                let _ = self.kill();
                false
            }
            // An error means no such child is left to collect.
            Ok(Some(_)) | Err(_) => {
                self.child = None;
                true
            }
        }
    }

    /// Generation teardown's end of a store inspection: the child killed and
    /// waited for before the generation's cleanup runs, on the peer path
    /// that has already failed.
    pub fn reap_for_teardown(mut self) {
        self.wire = None;
        let _ = self.kill();
        if let Some(child) = &mut self.child {
            let _ = child.wait();
        }
        self.child = None;
    }
}

/// Never blocks. Each owner keeps a helper until it is reaped (the login
/// state's retained run, or the store inspection that teardown reaps), so
/// this meets an unreaped one only as td-authd's generation ends, where a
/// wait on a helper the kernel cannot kill would wedge td-authd itself.
impl Drop for Inspection {
    fn drop(&mut self) {
        self.wire = None;
        let _ = self.kill();
        if let Some(child) = &mut self.child {
            let _ = child.try_wait();
        }
    }
}

#[cfg(test)]
#[path = "../tests/inspection.rs"]
pub(crate) mod tests;
