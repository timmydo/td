//! Named sandboxes used by another human-owned frontend. The profile reader,
//! root intake and relay are shared with the command-line frontend.
use crate::{failure, intake, owned, refused, state, wire, workload_accounts};
use intake::Intake;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The sandbox's configuration at the start of a turn. Root reads its grants
/// and network again at every launch; no caller supplies replacement grants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sandbox {
    pub name: String,
    pub runtime: Option<String>,
    pub home: PathBuf,
    pub host_network: bool,
    pub shares: Vec<(PathBuf, bool)>,
}

pub fn valid_name(name: &str) -> bool {
    state::name(name).is_ok()
}

impl Sandbox {
    /// Read an existing profile and ask root for its account and grants.
    /// This creates no profile, account, directory or grant.
    pub fn load(human: &Path, name: &str) -> io::Result<Self> {
        let human = std::fs::canonicalize(human)?;
        let profiles = human.join(".local/share/td-sandbox/.profiles");
        let profile = state::Profile::read(&profiles, name)?;
        let grants = intake::Root::new()
            .ask(&intake::simple("grants", name)?)?
            .accepted()
            .and_then(|answer| intake::grants(&answer))?;
        Self::from_grants(name, profile.runtime, &human, grants)
    }

    fn from_grants(
        name: &str,
        runtime: Option<String>,
        human: &Path,
        grants: intake::Grants,
    ) -> io::Result<Self> {
        if !workload_accounts::WORKLOAD_UIDS.contains(&grants.uid) {
            return Err(io::Error::other("sandbox has no workload account"));
        }
        let mut shares = Vec::new();
        for (path, write) in grants.directories {
            if path.is_empty()
                || Path::new(&path)
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                return Err(io::Error::other("sandbox grant is not home-relative"));
            }
            td_alloc::push(&mut shares, (human.join(path), write)).map_err(refused)?;
        }
        Ok(Self {
            name: owned(name)?,
            runtime,
            home: workload_accounts::Kind::Sandbox.home(grants.uid),
            host_network: grants.host,
            shares,
        })
    }

    /// Run a command through the existing sandbox relay in pipes mode.
    /// Multiple agent tool instances may share the account; each has its own
    /// jail and channel. Dropping/shutting the channel ends that instance.
    pub fn launch(&self, command: &[String]) -> io::Result<(Output, Input, UnixStream)> {
        state::name(&self.name)?;
        if let Some(runtime) = &self.runtime {
            state::runtime(runtime)?;
        }
        let line = intake::run(&self.name, self.runtime.as_deref(), false, "dumb", command)?;
        let channel = intake::Root::new().launch(&line)?;
        channel.set_write_timeout(Some(Duration::from_millis(250)))?;
        let input = Input(channel.try_clone()?);
        let output = Output {
            channel: channel.try_clone()?,
            pending: io::Cursor::new(Vec::new()),
            diagnostic: Vec::new(),
            ended: false,
        };
        Ok((output, input, channel))
    }
}

/// Raw command input framed for td-sandbox. A partial frame failure is fatal:
/// a caller must never retry its payload as a second relay frame.
pub struct Input(UnixStream);
impl Write for Input {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let bytes = bytes
            .get(..bytes.len().min(wire::LIMIT))
            .unwrap_or_default();
        if bytes.is_empty() {
            return Ok(0);
        }
        wire::send(&mut self.0, wire::INPUT, bytes).map_err(|_| {
            let _ = self.0.shutdown(std::net::Shutdown::Both);
            io::Error::from(io::ErrorKind::BrokenPipe)
        })?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Only stdout reaches the tool protocol. Stderr stays bounded and is included
/// in a failed exit or malformed/closed relay diagnostic.
pub struct Output {
    channel: UnixStream,
    pending: io::Cursor<Vec<u8>>,
    diagnostic: Vec<u8>,
    ended: bool,
}
impl Read for Output {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        loop {
            let count = self.pending.read(bytes)?;
            if count != 0 || self.ended {
                return Ok(count);
            }
            let (kind, payload) = wire::read(&mut self.channel).map_err(|e| {
                failure(format_args!(
                    "sandbox relay ended: {e}; {}; see {}",
                    std::str::from_utf8(&self.diagnostic).unwrap_or("non-UTF-8 stderr"),
                    intake::LOG
                ))
            })?;
            match kind {
                wire::OUTPUT => self.pending = io::Cursor::new(payload),
                wire::ERROR => {
                    let kept = payload
                        .get(payload.len().saturating_sub(4096)..)
                        .unwrap_or_default();
                    let discard = (self.diagnostic.len() + kept.len()).saturating_sub(4096);
                    self.diagnostic.drain(..discard);
                    td_alloc::extend_from_slice(&mut self.diagnostic, kept).map_err(refused)?;
                }
                wire::EXIT if payload.len() == 1 => {
                    self.ended = true;
                    if payload.first() != Some(&0) {
                        return Err(failure(format_args!(
                            "sandbox command exited {:?}: {}",
                            payload.first(),
                            std::str::from_utf8(&self.diagnostic).unwrap_or("non-UTF-8 stderr")
                        )));
                    }
                }
                _ => return Err(io::Error::other("invalid sandbox output frame")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relay_preserves_binary_streams_and_keeps_stderr_out() -> io::Result<()> {
        let (ours, mut theirs) = UnixStream::pair()?;
        let mut output = Output {
            channel: ours.try_clone()?,
            pending: io::Cursor::new(Vec::new()),
            diagnostic: Vec::new(),
            ended: false,
        };
        let mut input = Input(ours);
        input.write_all(b"\0hello\n")?;
        assert_eq!(
            wire::read(&mut theirs)?,
            (wire::INPUT, b"\0hello\n".to_vec())
        );
        wire::send(&mut theirs, wire::OUTPUT, b"")?;
        wire::send(&mut theirs, wire::ERROR, b"diagnostic")?;
        wire::send(&mut theirs, wire::OUTPUT, b"\0reply")?;
        wire::send(&mut theirs, wire::EXIT, &[0])?;
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes)?;
        assert_eq!(bytes, b"\0reply");
        assert_eq!(output.diagnostic, b"diagnostic");
        Ok(())
    }
    #[test]
    fn relay_errors_and_unreported_end_never_become_clean_eof() -> io::Result<()> {
        for frame in [
            (wire::EXIT, vec![7]),
            (wire::EXIT, vec![]),
            (wire::INPUT, vec![1]),
        ] {
            let (ours, mut theirs) = UnixStream::pair()?;
            wire::send(&mut theirs, wire::ERROR, &vec![b'x'; wire::LIMIT])?;
            wire::send(&mut theirs, frame.0, &frame.1)?;
            drop(theirs);
            let mut output = Output {
                channel: ours,
                pending: io::Cursor::new(Vec::new()),
                diagnostic: Vec::new(),
                ended: false,
            };
            assert!(output.read_to_end(&mut Vec::new()).is_err());
            assert_eq!(output.diagnostic.len(), 4096);
        }
        let (ours, theirs) = UnixStream::pair()?;
        drop(theirs);
        let mut input = Input(ours.try_clone()?);
        assert_eq!(
            input.write(b"call").err().map(|e| e.kind()),
            Some(io::ErrorKind::BrokenPipe)
        );
        let mut output = Output {
            channel: ours,
            pending: io::Cursor::new(Vec::new()),
            diagnostic: Vec::new(),
            ended: false,
        };
        assert!(output.read(&mut [0]).is_err());
        Ok(())
    }

    #[test]
    fn stderr_reservation_refusal_is_reported_without_growing() -> io::Result<()> {
        let (ours, mut theirs) = UnixStream::pair()?;
        wire::send(&mut theirs, wire::ERROR, b"diagnostic")?;
        let mut output = Output {
            channel: ours,
            pending: io::Cursor::new(Vec::new()),
            diagnostic: Vec::new(),
            ended: false,
        };
        let result = td_alloc::with_reservation_limit(0, || output.read(&mut [0]));
        assert_eq!(
            result.err().map(|e| e.kind()),
            Some(io::ErrorKind::OutOfMemory)
        );
        assert!(output.diagnostic.is_empty());
        wire::send(&mut theirs, wire::OUTPUT, b"next")?;
        assert_eq!(output.read(&mut [0])?, 1);
        Ok(())
    }

    #[test]
    fn a_missing_profile_is_not_created() -> io::Result<()> {
        let home = td_fs::TempDir::new("sandbox-client-missing")?;
        assert!(Sandbox::load(&home, "absent").is_err());
        assert!(!home.join(".local").exists());
        assert!(Sandbox::load(&home, "../outside").is_err());
        Ok(())
    }

    #[test]
    fn account_home_and_shares_come_from_root() -> io::Result<()> {
        let grants = intake::grants("98304 off 2\nro\tPictures\nrw\tsandbox\n")?;
        let sandbox = Sandbox::from_grants(
            "default",
            Some("freedesktop-platform-25-08".into()),
            Path::new("/home/person"),
            grants,
        )?;
        assert_eq!(sandbox.home, Path::new("/var/lib/td/workloads/98304"));
        assert_eq!(
            sandbox.shares,
            [
                (PathBuf::from("/home/person/Pictures"), false),
                (PathBuf::from("/home/person/sandbox"), true)
            ]
        );
        assert!(!sandbox.host_network);
        Ok(())
    }
}
