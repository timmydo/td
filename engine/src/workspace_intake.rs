//! Shared client transport for root's workspace intake. A launch consumes
//! exactly its answer line; every following byte belongs to the workload.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

fn refused<E>(_: E) -> io::Error {
    io::ErrorKind::OutOfMemory.into()
}
fn text(args: std::fmt::Arguments<'_>) -> io::Result<String> {
    td_alloc::format(args).map_err(refused)
}
fn owned(value: &str) -> io::Result<String> {
    td_alloc::string(value).map_err(refused)
}
fn failure(args: std::fmt::Arguments<'_>) -> io::Error {
    text(args).map_or_else(|e| e, io::Error::other)
}

/// Where root's intake listens; the primary human alone may connect.
pub const SOCKET: &str = crate::workload_accounts::WORKSPACE_INTAKE;
/// Where root reports what happens after a launch's answer, and why one
/// that ended before its relay began did not start.
pub const LOG: &str = "/var/log/svc/workspace-intake.log";
/// The longest answer read: a listing of an account's every directory row
/// at the longest path.
const MAX_ANSWER: usize = 64 * 1024;
/// A launch's answer, `ok` or a refusal, read byte by byte so nothing of
/// the relay after it is taken.
const MAX_LINE: usize = 256;
/// How long root may take to answer: a launch prepares the account and
/// publishes its views; a deletion has the account empty its home.
const LAUNCH_PATIENCE: Duration = Duration::from_secs(120);
const ANSWER_PATIENCE: Duration = Duration::from_secs(600);
/// Root's refusals by their words: a creation over a sandbox that holds
/// files or grants, and a launch or grant change of a deleted one.
pub const EXISTS: &str = "exists";
pub const DELETED: &str = "deleted";
/// Before a refusal asking again cannot change: root's or the writer's
/// refusal of the request itself. Any other may pass.
const FINAL: &str = "final: ";

/// The reason of a refusal that asking again cannot change, none for one
/// that may pass.
pub fn lasting(reason: &str) -> Option<&str> {
    reason.strip_prefix(FINAL)
}

/// A refusal's reason as the human reads it.
pub fn shown(reason: &str) -> &str {
    lasting(reason).unwrap_or(reason)
}

/// An answer: `ok` and what follows it, or the reason root refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Ok(String),
    Refused(String),
}

impl Answer {
    /// What followed `ok`, or the refusal as an error.
    pub fn accepted(self) -> io::Result<String> {
        match self {
            Self::Ok(rest) => Ok(rest),
            Self::Refused(reason) => Err(io::Error::other(shown(&reason))),
        }
    }
}

/// Root's intake as td-sandbox asks it, or a stand-in in the tests.
pub trait Intake {
    /// One request that root answers whole and closes.
    fn ask(&self, line: &str) -> io::Result<Answer>;
}

/// The intake at a socket path.
pub struct Root<'a, const MAX_REQUEST: usize> {
    pub socket: &'a Path,
    /// How long a connection may wait for the socket to appear: at boot,
    /// the intake's service may still be binding it.
    pub patience: Duration,
}

impl<const MAX_REQUEST: usize> Root<'_, MAX_REQUEST> {
    pub fn new() -> Self {
        Self {
            socket: Path::new(SOCKET),
            patience: Duration::ZERO,
        }
    }

    fn connect(&self) -> io::Result<UnixStream> {
        let deadline = Instant::now() + self.patience;
        loop {
            match td_fs::unix_connect_until(self.socket, Instant::now() + Duration::from_secs(2)) {
                Ok(stream) => return Ok(stream),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(e) => {
                    let message = text(format_args!(
                        "root's workspace intake at {} cannot be reached ({e}); \
                         workloads need td's workspace-intake service",
                        self.socket.display()
                    ))?;
                    return Err(io::Error::new(e.kind(), message));
                }
            }
        }
    }

    /// Sends a launch request and reads its one answer line: on `ok` the
    /// connection, which is now the relay's.
    pub fn launch(&self, line: &str) -> io::Result<UnixStream> {
        let mut stream = self.connect()?;
        send::<MAX_REQUEST>(&mut stream, line)?;
        let deadline = Instant::now() + LAUNCH_PATIENCE;
        let mut answer = td_alloc::with_capacity(MAX_LINE).map_err(refused)?;
        let mut byte = [0u8; 1];
        while answer.last() != Some(&b'\n') {
            if answer.len() >= MAX_LINE {
                return Err(io::Error::other("root's workspace intake answered amiss"));
            }
            stream.set_read_timeout(Some(remaining(deadline)?))?;
            match stream.read(&mut byte) {
                Ok(0) => {
                    return Err(failure(format_args!(
                        "root's workspace intake closed without an answer; {LOG} says why"
                    )))
                }
                Ok(_) => td_alloc::extend_from_slice(&mut answer, &byte).map_err(refused)?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        stream.set_read_timeout(None)?;
        match parse(&answer)? {
            Answer::Ok(rest) if rest.is_empty() => Ok(stream),
            Answer::Ok(_) => Err(io::Error::other("root's workspace intake answered amiss")),
            Answer::Refused(reason) => Err(io::Error::other(shown(&reason))),
        }
    }
}

impl<const MAX_REQUEST: usize> Intake for Root<'_, MAX_REQUEST> {
    fn ask(&self, line: &str) -> io::Result<Answer> {
        let mut stream = self.connect()?;
        send::<MAX_REQUEST>(&mut stream, line)?;
        let deadline = Instant::now() + ANSWER_PATIENCE;
        // Reserved whole, so reading to the bound never grows it.
        let mut answer = td_alloc::with_capacity(MAX_ANSWER + 1).map_err(refused)?;
        let mut chunk = [0u8; 4096];
        while answer.len() <= MAX_ANSWER {
            stream.set_read_timeout(Some(remaining(deadline)?))?;
            let bound = chunk.len().min(MAX_ANSWER + 1 - answer.len());
            let buffer = chunk
                .get_mut(..bound)
                .ok_or_else(|| io::Error::other("answer bound"))?;
            match stream.read(buffer) {
                Ok(0) => break,
                Ok(n) => td_alloc::extend_from_slice(
                    &mut answer,
                    buffer
                        .get(..n)
                        .ok_or_else(|| io::Error::other("answer read"))?,
                )
                .map_err(refused)?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        if answer.len() > MAX_ANSWER {
            return Err(io::Error::other(
                "root's workspace intake answered too much",
            ));
        }
        parse(&answer)
    }
}

fn send<const MAX_REQUEST: usize>(stream: &mut UnixStream, line: &str) -> io::Result<()> {
    if line.len() > MAX_REQUEST || !line.ends_with('\n') {
        return Err(io::Error::other(
            "the request is too long for root's intake",
        ));
    }
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(line.as_bytes())?;
    stream.set_write_timeout(None)
}

/// `ok`, alone or with a space and more, or `refused` and a reason, one
/// line or, for `ok`, more.
pub fn parse(answer: &[u8]) -> io::Result<Answer> {
    let amiss = || io::Error::other("root's workspace intake answered amiss");
    let text = std::str::from_utf8(answer).map_err(|_| amiss())?;
    if let Some(reason) = text
        .strip_prefix("refused ")
        .and_then(|rest| rest.strip_suffix('\n'))
    {
        if reason.is_empty() || reason.contains('\n') {
            return Err(amiss());
        }
        return Ok(Answer::Refused(owned(reason)?));
    }
    match text.strip_prefix("ok") {
        Some("\n") => Ok(Answer::Ok(String::new())),
        Some(rest) => match rest.strip_prefix(' ').filter(|rest| rest.ends_with('\n')) {
            Some(rest) => Ok(Answer::Ok(owned(rest)?)),
            None => Err(amiss()),
        },
        None => Err(amiss()),
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| io::ErrorKind::TimedOut.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn launch_refuses_malformed_long_and_missing_answers() -> io::Result<()> {
        let dir = td_fs::TempDir::new("workspace-transport")?;
        let path = dir.join("socket");
        let listener = UnixListener::bind(&path)?;
        let root = Root::<8192> {
            socket: &path,
            patience: Duration::ZERO,
        };
        let server = std::thread::spawn(move || -> io::Result<()> {
            for answer in [
                b"ok extra\n".to_vec(),
                vec![b'a'; MAX_LINE],
                b"ok \xff\n".to_vec(),
                vec![],
                b"refused final: bad plan\n".to_vec(),
            ] {
                let (mut stream, _) = listener.accept()?;
                let mut request = [0; 2];
                stream.read_exact(&mut request)?;
                assert_eq!(&request, b"x\n");
                stream.write_all(&answer)?;
            }
            Ok(())
        });
        for _ in 0..3 {
            assert!(root.launch("x\n").is_err());
        }
        let missing = root
            .launch("x\n")
            .err()
            .ok_or_else(|| io::Error::other("accepted EOF"))?;
        assert!(missing.to_string().contains(LOG));
        let refused = root
            .launch("x\n")
            .err()
            .ok_or_else(|| io::Error::other("accepted refusal"))?;
        assert_eq!(refused.to_string(), "bad plan");
        server
            .join()
            .map_err(|_| io::Error::other("server panicked"))??;
        Ok(())
    }

    #[test]
    fn answer_storage_and_request_sizes_are_bounded() -> io::Result<()> {
        let dir = td_fs::TempDir::new("workspace-answer")?;
        let path = dir.join("socket");
        let listener = UnixListener::bind(&path)?;
        let root = Root::<8192> {
            socket: &path,
            patience: Duration::ZERO,
        };
        let server = std::thread::spawn(move || -> io::Result<()> {
            let (mut stream, _) = listener.accept()?;
            let mut request = [0; 2];
            stream.read_exact(&mut request)?;
            stream.write_all(&vec![b'x'; MAX_ANSWER + 1])
        });
        assert!(root.ask("x\n").is_err());
        server
            .join()
            .map_err(|_| io::Error::other("server panicked"))??;
        let (mut a, mut b) = UnixStream::pair()?;
        assert!(send::<2>(&mut a, "abc\n").is_err());
        assert!(send::<2>(&mut a, "xx").is_err());
        send::<2>(&mut a, "x\n")?;
        let mut got = [0; 2];
        b.read_exact(&mut got)?;
        assert_eq!(&got, b"x\n");
        assert!(remaining(Instant::now()).is_err());
        Ok(())
    }
}
