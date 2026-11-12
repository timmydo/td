//! td-sandbox's half of root's workspace launch intake (td-authd/DESIGN.md,
//! "Workspace launch intake"): one exact request line per connection, and
//! one answer, `ok` with what it found or did, or `refused` and why. A
//! launch's `ok` hands the connection on to the relay.

use crate::{failure, owned, refused, text};
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where root's intake listens; the primary human alone may connect.
pub const SOCKET: &str = crate::workload_accounts::WORKSPACE_INTAKE;
/// Where root reports what happens after a launch's answer, and why one
/// that ended before its relay began did not start.
pub const LOG: &str = "/var/log/svc/workspace-intake.log";
/// The intake's longest request, its newline included.
const MAX_REQUEST: usize = 8192;
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
pub struct Root {
    pub socket: PathBuf,
    /// How long a connection may wait for the socket to appear: at boot,
    /// the intake's service may still be binding it.
    pub patience: Duration,
}

impl Root {
    pub fn new() -> Self {
        Self {
            socket: PathBuf::from(SOCKET),
            patience: Duration::ZERO,
        }
    }

    fn connect(&self) -> io::Result<UnixStream> {
        let deadline = Instant::now() + self.patience;
        loop {
            match UnixStream::connect(&self.socket) {
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
                         sandboxes run only on td, through its workspace-intake service",
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
        send(&mut stream, line)?;
        stream.set_read_timeout(Some(LAUNCH_PATIENCE))?;
        let mut answer = td_alloc::with_capacity(MAX_LINE).map_err(refused)?;
        let mut byte = [0u8; 1];
        while answer.last() != Some(&b'\n') {
            if answer.len() >= MAX_LINE {
                return Err(io::Error::other("root's workspace intake answered amiss"));
            }
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

impl Intake for Root {
    fn ask(&self, line: &str) -> io::Result<Answer> {
        let mut stream = self.connect()?;
        send(&mut stream, line)?;
        stream.set_read_timeout(Some(ANSWER_PATIENCE))?;
        // Reserved whole, so reading to the bound never grows it.
        let mut answer = td_alloc::with_capacity(MAX_ANSWER + 1).map_err(refused)?;
        stream
            .take(MAX_ANSWER as u64 + 1)
            .read_to_end(&mut answer)?;
        if answer.len() > MAX_ANSWER {
            return Err(io::Error::other(
                "root's workspace intake answered too much",
            ));
        }
        parse(&answer)
    }
}

fn send(stream: &mut UnixStream, line: &str) -> io::Result<()> {
    if line.len() > MAX_REQUEST || !line.ends_with('\n') {
        return Err(io::Error::other(
            "the request is too long for root's intake",
        ));
    }
    stream.write_all(line.as_bytes())
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

/// `start NAME RUNTIME TERM`: td's shell on a PTY.
pub fn start(name: &str, runtime: Option<&str>, term: &str) -> io::Result<String> {
    text(format_args!(
        "start {name} {} {term}\n",
        runtime.unwrap_or("native")
    ))
}

/// `run NAME RUNTIME pty|pipes TERM WORD...`, each word `x` and its
/// argument's bytes in lowercase hex, so any argument is one word.
pub fn run(
    name: &str,
    runtime: Option<&str>,
    terminal: bool,
    term: &str,
    command: &[String],
) -> io::Result<String> {
    let mut line = text(format_args!(
        "run {name} {} {} {term}",
        runtime.unwrap_or("native"),
        if terminal { "pty" } else { "pipes" }
    ))?;
    for argument in command {
        td_alloc::push_str(&mut line, " x").map_err(refused)?;
        // Reserved first, so writing the digits never grows the line.
        td_alloc::reserve_string(&mut line, argument.len().saturating_mul(2)).map_err(refused)?;
        for byte in argument.bytes() {
            write!(line, "{byte:02x}").map_err(refused)?;
        }
    }
    td_alloc::push_str(&mut line, "\n").map_err(refused)?;
    if line.len() > MAX_REQUEST {
        return Err(failure(format_args!(
            "the command is too long: root's intake takes {MAX_REQUEST} bytes, \
             each argument byte as two"
        )));
    }
    Ok(line)
}

/// `create NAME`, `delete NAME`, `status NAME` or `grants NAME`.
pub fn simple(verb: &str, name: &str) -> io::Result<String> {
    text(format_args!("{verb} {name}\n"))
}

/// `grant NAME ro|rw PATH`, PATH relative to the human's home.
pub fn grant(name: &str, write: bool, path: &str) -> io::Result<String> {
    text(format_args!(
        "grant {name} {} {path}\n",
        if write { "rw" } else { "ro" }
    ))
}

/// `revoke NAME PATH`.
pub fn revoke(name: &str, path: &str) -> io::Result<String> {
    text(format_args!("revoke {name} {path}\n"))
}

/// `network NAME off|host`.
pub fn network(name: &str, host: bool) -> io::Result<String> {
    text(format_args!(
        "network {name} {}\n",
        if host { "host" } else { "off" }
    ))
}

/// A created sandbox's account uid, from `ok UID`.
pub fn created(rest: &str) -> io::Result<u32> {
    rest.strip_suffix('\n')
        .and_then(decimal)
        .ok_or_else(|| io::Error::other("root's workspace intake answered amiss"))
}

/// The uid `text` spells in canonical decimal, as root writes it.
fn decimal(text: &str) -> Option<u32> {
    crate::workload_accounts::canonical_decimal(text)
        .then(|| text.parse::<u32>().ok())
        .flatten()
}

/// What root holds of a sandbox, as `status` answers it.
#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    /// No account: the ledger holds no sandbox of the name.
    Absent,
    /// An account holding no file and no grant row, a deleted sandbox's
    /// among them: creating it loses nothing.
    Free(u32),
    /// An account holding files or grant rows: a creation is refused, and
    /// a record of it is made by adopting it.
    Held(u32),
}

/// `absent`, `free UID` or `held UID`.
pub fn status(rest: &str) -> io::Result<Status> {
    let amiss = || io::Error::other("root's workspace intake answered amiss");
    let line = rest.strip_suffix('\n').ok_or_else(amiss)?;
    if line == "absent" {
        return Ok(Status::Absent);
    }
    let (state, uid) = line.split_once(' ').ok_or_else(amiss)?;
    let uid = decimal(uid).ok_or_else(amiss)?;
    match state {
        "free" => Ok(Status::Free(uid)),
        "held" => Ok(Status::Held(uid)),
        _ => Err(amiss()),
    }
}

/// A sandbox's grants as a listing gives them.
#[derive(Debug, PartialEq, Eq)]
pub struct Grants {
    pub uid: u32,
    pub host: bool,
    /// Each row's path relative to the human's home, and whether it is
    /// writable.
    pub directories: Vec<(String, bool)>,
}

/// `ok UID NETWORK COUNT` and COUNT `ACCESS<TAB>PATH` lines.
pub fn grants(rest: &str) -> io::Result<Grants> {
    let amiss = || io::Error::other("root's workspace intake answered amiss");
    let (head, rows) = rest.split_once('\n').ok_or_else(amiss)?;
    let mut words = head.split(' ');
    let (Some(uid), Some(network), Some(count), None) =
        (words.next(), words.next(), words.next(), words.next())
    else {
        return Err(amiss());
    };
    let uid = uid.parse::<u32>().map_err(|_| amiss())?;
    let host = match network {
        "host" => true,
        "off" => false,
        _ => return Err(amiss()),
    };
    let count = count.parse::<usize>().map_err(|_| amiss())?;
    let mut directories = Vec::new();
    for row in rows.split_terminator('\n') {
        let (access, path) = row.split_once('\t').ok_or_else(amiss)?;
        let write = match access {
            "rw" => true,
            "ro" => false,
            _ => return Err(amiss()),
        };
        td_alloc::push(&mut directories, (owned(path)?, write)).map_err(refused)?;
    }
    if directories.len() != count || !(rows.is_empty() || rows.ends_with('\n')) {
        return Err(amiss());
    }
    Ok(Grants {
        uid,
        host,
        directories,
    })
}

/// `path`, an absolute directory of the human's, relative to the human's
/// `home` as both are canonical: the form a grant row holds. The home
/// itself and anything outside it have none.
pub fn relative<'a>(home: &Path, path: &'a Path) -> Option<&'a str> {
    let relative = path.strip_prefix(home).ok()?;
    relative.to_str().filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each request is the exact line td-authd's intake parses (its own
    /// tests hold the same lines).
    #[test]
    fn requests_are_the_lines_the_intake_parses() -> io::Result<()> {
        assert_eq!(
            start("default", None, "xterm-256color")?,
            "start default native xterm-256color\n"
        );
        assert_eq!(
            start("tools", Some("freedesktop-sdk-25-08"), "linux")?,
            "start tools freedesktop-sdk-25-08 linux\n"
        );
        assert_eq!(
            run(
                "oracle",
                None,
                false,
                "dumb",
                &[
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo 'a b'\nexit 3".into(),
                    String::new()
                ]
            )?,
            "run oracle native pipes dumb x2f62696e2f7368 x2d63 x6563686f202761206227\
             0a657869742033 x\n"
        );
        assert_eq!(
            run(
                "tools",
                Some("freedesktop-sdk-25-08"),
                true,
                "xterm",
                &["ls".into()]
            )?,
            "run tools freedesktop-sdk-25-08 pty xterm x6c73\n"
        );
        assert!(run("x", None, false, "dumb", &["a".repeat(MAX_REQUEST)]).is_err());
        assert_eq!(simple("create", "default")?, "create default\n");
        assert_eq!(simple("delete", "default")?, "delete default\n");
        assert_eq!(simple("grants", "default")?, "grants default\n");
        assert_eq!(
            grant("default", true, "src/my project")?,
            "grant default rw src/my project\n"
        );
        assert_eq!(grant("default", false, "src")?, "grant default ro src\n");
        assert_eq!(revoke("default", "src")?, "revoke default src\n");
        assert_eq!(network("default", true)?, "network default host\n");
        assert_eq!(network("default", false)?, "network default off\n");
        Ok(())
    }

    #[test]
    fn answers_are_ok_or_a_refusal_and_nothing_else() -> io::Result<()> {
        assert_eq!(parse(b"ok\n")?, Answer::Ok(String::new()));
        assert_eq!(parse(b"ok 98304\n")?, Answer::Ok("98304\n".into()));
        assert_eq!(
            parse(b"refused sandbox x has no account\n")?,
            Answer::Refused("sandbox x has no account".into())
        );
        for amiss in [
            &b""[..],
            b"ok",
            b"ok98304\n",
            b"okay\n",
            b"refused \n",
            b"refused a\nb\n",
            b"refused a",
            b"no\n",
            b"ok \xff\n",
        ] {
            assert!(parse(amiss).is_err(), "{amiss:?}");
        }
        assert_eq!(created("98304\n")?, 98304);
        assert_eq!(status("absent\n")?, Status::Absent);
        assert_eq!(status("free 98304\n")?, Status::Free(98304));
        assert_eq!(status("held 98305\n")?, Status::Held(98305));
        for amiss in ["absent", "held\n", "held x\n", "gone 1\n", "free 01\n"] {
            assert!(status(amiss).is_err(), "{amiss:?}");
        }
        for amiss in ["98304", "098304\n", "x\n", "\n"] {
            assert!(created(amiss).is_err(), "{amiss:?}");
        }
        assert_eq!(
            grants("98304 host 2\nro\tMy Pictures\nrw\tsrc/td\n")?,
            Grants {
                uid: 98304,
                host: true,
                directories: vec![("My Pictures".into(), false), ("src/td".into(), true)],
            }
        );
        assert_eq!(
            grants("98305 off 0\n")?,
            Grants {
                uid: 98305,
                host: false,
                directories: vec![],
            }
        );
        for amiss in [
            "98304 host 1\n",
            "98304 host 1\nrx\ta\n",
            "98304 on 0\n",
            "98304 host\n",
            "98304 off 1\nro\ta",
            "x off 0\n",
        ] {
            assert!(grants(amiss).is_err(), "{amiss:?}");
        }
        Ok(())
    }

    /// A grant's path is relative to the canonical home, below it; the
    /// home itself and anything beside it have no grant form.
    #[test]
    fn only_a_directory_below_the_home_has_a_grant_form() {
        let home = Path::new("/var/home/tester");
        assert_eq!(
            relative(home, Path::new("/var/home/tester/sandbox")),
            Some("sandbox")
        );
        assert_eq!(
            relative(home, Path::new("/var/home/tester/src/my project")),
            Some("src/my project")
        );
        for outside in [
            "/var/home/tester",
            "/var/home/testers/x",
            "/home/tester/x",
            "/srv/data",
            "/",
        ] {
            assert_eq!(relative(home, Path::new(outside)), None, "{outside}");
        }
    }

    /// A launch whose connection ends before its answer, or whose answer
    /// refuses, never reaches the relay; `ok` hands the connection on.
    #[test]
    fn a_launch_takes_only_its_answer_line() -> io::Result<()> {
        let dir = td_fs::TempDir::new("sandbox-intake")?;
        let socket = dir.join("workspace");
        let listener = std::os::unix::net::UnixListener::bind(&socket)?;
        let root = Root {
            socket: socket.clone(),
            patience: Duration::ZERO,
        };
        let server = std::thread::spawn(move || -> io::Result<Vec<Vec<u8>>> {
            let mut seen = Vec::new();
            for answer in [&b"ok\n\x04"[..], b"refused no such sandbox\n", b""] {
                let (mut stream, _) = listener.accept()?;
                let mut line = Vec::new();
                let mut byte = [0u8; 1];
                while line.last() != Some(&b'\n') {
                    stream.read_exact(&mut byte)?;
                    let [read] = byte;
                    line.push(read);
                }
                seen.push(line);
                stream.write_all(answer)?;
            }
            Ok(seen)
        });
        let mut relay = root.launch("start default native xterm\n")?;
        let mut first = [0u8; 1];
        relay.read_exact(&mut first)?;
        assert_eq!(first, [4]);
        let refused = root.launch("start default native xterm\n").err();
        assert_eq!(
            refused.map(|e| e.to_string()).as_deref(),
            Some("no such sandbox")
        );
        let closed = root
            .launch("start default native xterm\n")
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(closed.contains(LOG), "{closed}");
        let seen = server
            .join()
            .map_err(|_| io::Error::other("server panicked"))??;
        assert_eq!(seen.len(), 3);
        // No intake at all names what is missing.
        let missing = Root {
            socket: dir.join("absent"),
            patience: Duration::ZERO,
        };
        let error = missing.ask("create x\n").err().map(|e| e.to_string());
        assert!(error.unwrap_or_default().contains("workspace-intake"));
        Ok(())
    }
}
