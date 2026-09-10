//! Fixed Claude shell entry: root binds; only the application UID serves.
use crate::{shell_channel::Channel, terminal, terminal_sys};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

pub(crate) const UID: u32 = 65539;
const OWNER: u32 = 1000;
const SOCKET: &str = "/run/td-claude-launch";
const HOME: &str = "/var/lib/td/applications/65539";
const SESSIONS: usize = 16;

pub(crate) fn bind() -> io::Result<Stdio> {
    let parent = fs::symlink_metadata("/run")?;
    if !parent.is_dir() || parent.uid() != 0 || parent.mode() & 0o022 != 0 {
        return Err(io::Error::other("untrusted Claude launch socket parent"));
    }
    match fs::symlink_metadata(SOCKET) {
        Ok(meta) => {
            if !meta.file_type().is_socket() || meta.uid() != OWNER || meta.gid() != OWNER {
                return Err(io::Error::other("unexpected Claude launch socket inode"));
            }
            match UnixStream::connect(SOCKET) {
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    fs::remove_file(SOCKET)?
                }
                _ => return Err(io::Error::other("Claude launch socket is already active")),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(SOCKET)?;
    fs::set_permissions(SOCKET, fs::Permissions::from_mode(0o600))?;
    std::os::unix::fs::chown(SOCKET, Some(OWNER), Some(OWNER))?;
    Ok(Stdio::from(OwnedFd::from(listener)))
}

struct Request {
    cwd: PathBuf,
    term: OsString,
    size: [u16; 4],
    arguments: Vec<OsString>,
}

fn field(bytes: &mut &[u8]) -> io::Result<Vec<u8>> {
    let size = bytes
        .get(..4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or_else(|| io::Error::other("truncated Claude launch field"))? as usize;
    let value = bytes
        .get(4..4usize.saturating_add(size))
        .ok_or_else(|| io::Error::other("truncated Claude launch value"))?;
    if value.contains(&0) {
        return Err(io::Error::other("NUL in Claude launch value"));
    }
    let value = value.to_vec();
    *bytes = bytes
        .get(4usize.saturating_add(size)..)
        .ok_or_else(|| io::Error::other("invalid launch cursor"))?;
    Ok(value)
}

fn append(bytes: &mut Vec<u8>, value: &OsStr) -> io::Result<()> {
    let value = value.as_bytes();
    if value.len() > 32768 || value.contains(&0) {
        return Err(io::Error::other(
            "Claude launch argument is too large or contains NUL",
        ));
    }
    bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
    bytes.extend_from_slice(value);
    if bytes.len() > crate::shell_channel::LIMIT {
        return Err(io::Error::other("Claude launch request too large"));
    }
    Ok(())
}

fn workspace(cwd: &Path) -> io::Result<PathBuf> {
    if !cwd.is_absolute() || cwd.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(io::Error::other("invalid Claude launch working directory"));
    }
    for source in ["/var/home/tester/src", "/home/tester/src"] {
        if let Ok(relative) = cwd.strip_prefix(source) {
            return Ok(Path::new(HOME).join("src").join(relative));
        }
    }
    Ok(PathBuf::from("/"))
}

impl Request {
    fn decode(mut bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > crate::shell_channel::LIMIT {
            return Err(io::Error::other("Claude request too large"));
        }
        let cwd = field(&mut bytes)?;
        if cwd.len() > 4096 {
            return Err(io::Error::other("Claude working directory too long"));
        }
        let cwd = workspace(Path::new(OsStr::from_bytes(&cwd)))?;
        let term = field(&mut bytes)?;
        if term.is_empty()
            || term.len() > 64
            || !term
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(b))
        {
            return Err(io::Error::other("invalid Claude terminal name"));
        }
        let mut size = [0; 4];
        for slot in &mut size {
            *slot = bytes
                .get(..2)
                .and_then(|b| b.try_into().ok())
                .map(u16::from_be_bytes)
                .ok_or_else(|| io::Error::other("truncated Claude window size"))?;
            bytes = bytes
                .get(2..)
                .ok_or_else(|| io::Error::other("invalid window cursor"))?;
        }
        let mut arguments = Vec::new();
        let mut total = 0usize;
        while !bytes.is_empty() {
            let value = field(&mut bytes)?;
            total += value.len() + 1;
            if arguments.len() >= 128 || total > 32768 {
                return Err(io::Error::other("too many Claude arguments"));
            }
            arguments.push(OsString::from_vec(value));
        }
        Ok(Self {
            cwd,
            term: OsString::from_vec(term),
            size,
            arguments,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new("/bin/td-jail");
        command
            .args([
                "--internal-application-session",
                &std::process::id().to_string(),
                "/bin/claude",
            ])
            .args(&self.arguments)
            .env_clear()
            .current_dir(&self.cwd)
            .env("HOME", HOME)
            .env("USER", "tda65539")
            .env("LOGNAME", "tda65539")
            .env("PATH", "/bin")
            .env("SHELL", "/bin/false")
            .env("XDG_RUNTIME_DIR", "/run/user/65539")
            .env("TERM", &self.term);
        command
    }
}

struct Session {
    child: Child,
    channel: Channel,
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn accept(stream: UnixStream) -> io::Result<Option<Session>> {
    let mut channel = Channel::connect(stream, OWNER, true)?;
    let bytes = channel.receive()?;
    if bytes == [0] {
        channel.send(&[0])?;
        return Ok(None);
    }
    let Some((&1, body)) = bytes.split_first() else {
        return Err(io::Error::other("invalid Claude request"));
    };
    let request = Request::decode(body)?;
    channel.live()?;
    let (master, slave) = terminal::terminal_pair()?;
    terminal_sys::relay_window_set(master.as_fd(), &request.size)?;
    let child = request
        .command()
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave))
        .spawn()?;
    let mut session = Session { child, channel };
    session.channel.send_terminal(&master)?;
    Ok(Some(session))
}

fn supervise(mut session: Session) -> io::Result<()> {
    loop {
        if let Some(status) = session.child.try_wait()? {
            let code = status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
            return session.channel.send(&code.to_be_bytes());
        }
        session.channel.idle()?;
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[derive(Default)]
struct Workers(Vec<std::thread::JoinHandle<()>>);
impl Workers {
    fn start(
        &mut self,
        operation: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) -> io::Result<()> {
        self.0.retain(|worker| !worker.is_finished());
        if self.0.len() >= SESSIONS {
            return Err(io::Error::other(
                "Claude launcher is at its connection limit",
            ));
        }
        self.0.push(std::thread::Builder::new().spawn(move || {
            let _ = operation();
        })?);
        Ok(())
    }
}

pub(crate) fn serve() -> io::Result<()> {
    let listener = UnixListener::from(io::stdin().as_fd().try_clone_to_owned()?);
    if listener.local_addr()?.as_pathname() != Some(Path::new(SOCKET)) {
        return Err(io::Error::other("wrong Claude launch listener"));
    }
    listener.set_nonblocking(false)?;
    let mut workers = Workers::default();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // Admission and completion have separate bounded workers, all
                // started after the credential drop. The spawning worker stays
                // alive until its jail child has been killed/reaped.
                let _ = workers.start(move || {
                    if let Some(session) = accept(stream)? {
                        supervise(session)?;
                    }
                    Ok(())
                });
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                        | io::ErrorKind::ConnectionAborted
                ) || matches!(error.raw_os_error(), Some(12 | 23 | 24 | 105)) =>
            {
                // Resource pressure or an aborted arrival does not cancel
                // established sessions. Avoid spinning while it persists.
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error),
        }
    }
}

pub(crate) fn client(arguments: Vec<OsString>, probe: bool) -> io::Result<u8> {
    let stream = UnixStream::connect(SOCKET)
        .map_err(|e| io::Error::new(e.kind(), format!("connect to Claude launcher: {e}")))?;
    let mut channel = Channel::connect(stream, UID, false)?;
    if probe {
        channel.send(&[0])?;
        return if channel.receive()? == [0] {
            Ok(0)
        } else {
            Err(io::Error::other("invalid Claude launcher readiness"))
        };
    }
    let mut bytes = vec![1];
    append(&mut bytes, std::env::current_dir()?.as_os_str())?;
    append(
        &mut bytes,
        &std::env::var_os("TERM").unwrap_or_else(|| OsString::from("td-term")),
    )?;
    for value in terminal::initial_size()? {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    for argument in arguments {
        append(&mut bytes, &argument)?;
    }
    Request::decode(
        bytes
            .get(1..)
            .ok_or_else(|| io::Error::other("empty Claude request"))?,
    )?;
    channel.send(&bytes)?;
    let master = channel.receive_terminal()?;
    let metadata = master.metadata()?;
    if !metadata.file_type().is_char_device() || metadata.rdev() != 0x502 {
        return Err(io::Error::other(
            "Claude launcher returned a non-PTY descriptor",
        ));
    }
    terminal::relay(master)?;
    let status = channel.receive()?;
    let code = status
        .as_slice()
        .try_into()
        .ok()
        .map(i32::from_be_bytes)
        .ok_or_else(|| io::Error::other("invalid Claude exit status"))?;
    u8::try_from(code).map_err(|_| io::Error::other("Claude exit status out of range"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    fn request(cwd: &str, arguments: &[&OsStr]) -> Vec<u8> {
        let mut bytes = Vec::new();
        append(&mut bytes, OsStr::new(cwd)).unwrap();
        append(&mut bytes, OsStr::new("xterm-256color")).unwrap();
        for value in [24u16, 80, 0, 0] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        for arg in arguments {
            append(&mut bytes, arg).unwrap();
        }
        bytes
    }

    #[test]
    fn stalled_admission_does_not_delay_another_childs_exit_status() {
        let (mut stalled, _retained) = crate::shell_channel::tests::pair();
        let (server, mut client) = crate::shell_channel::tests::pair();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "application_shell::tests::completion_child",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let session = Session {
            child,
            channel: server,
        };
        let mut workers = Workers::default();
        workers
            .start(move || {
                stalled.receive()?;
                Ok(())
            })
            .unwrap();
        workers.start(move || supervise(session)).unwrap();
        assert_eq!(client.receive().unwrap(), 0i32.to_be_bytes());
        // Drain both owned workers after observing the timely status.
        for worker in workers.0 {
            worker.join().unwrap();
        }
    }

    #[test]
    fn completion_child() {}

    #[test]
    fn pending_connections_count_toward_the_worker_limit() {
        let mut workers = Workers::default();
        let (release, receive) = std::sync::mpsc::channel::<()>();
        let receive = std::sync::Arc::new(std::sync::Mutex::new(receive));
        for _ in 0..SESSIONS {
            let receive = receive.clone();
            workers
                .start(move || {
                    let _ = receive.lock().unwrap().recv();
                    Ok(())
                })
                .unwrap();
        }
        assert!(workers.start(|| Ok(())).is_err());
        drop(release);
        for worker in workers.0 {
            worker.join().unwrap();
        }
    }

    #[test]
    fn literal_request_retains_bytes_and_maps_only_the_granted_workspace() {
        let bytes = request(
            "/home/tester/src/td/.worktrees/fix",
            &[OsStr::new(";$(literal)"), OsStr::from_bytes(b"nonutf8\xff")],
        );
        let request = Request::decode(&bytes).unwrap();
        assert_eq!(request.cwd, Path::new(HOME).join("src/td/.worktrees/fix"));
        let command = request.command();
        assert_eq!(command.get_program(), "/bin/td-jail");
        assert_eq!(
            command.get_args().last().unwrap().as_bytes(),
            b"nonutf8\xff"
        );
        assert_eq!(
            workspace(Path::new("/var/home/tester/src/sub")).unwrap(),
            Path::new(HOME).join("src/sub")
        );
        assert_eq!(workspace(Path::new("/etc")).unwrap(), Path::new("/"));
        assert!(workspace(Path::new("/home/tester/src/../secret")).is_err());
        assert!(workspace(Path::new("relative")).is_err());
    }

    #[test]
    fn truncated_oversized_and_nul_requests_are_refused() {
        let bytes = request("/", &[]);
        for length in 0..bytes.len() {
            assert!(Request::decode(bytes.get(..length).unwrap()).is_err());
        }
        assert!(Request::decode(&request("/", &vec![OsStr::new("x"); 129])).is_err());
        assert!(Request::decode(&request("/", &[OsStr::new(&"x".repeat(32768))])).is_err());
        let mut bytes = request("/", &[]);
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.push(0);
        assert!(Request::decode(&bytes).is_err());
    }
}
