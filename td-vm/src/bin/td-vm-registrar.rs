//! Account-authenticated local operations on one host VM Git registry.
#![deny(unsafe_code)]

#[path = "../vm_registrar_sys.rs"]
mod sys;

#[path = "../vm_git_origin.rs"]
mod origin;

use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const HEADER: &str = "TDVM-REGISTRAR-1\n";
const MAX_FRAME: usize = 512;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
static SERIAL: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, PartialEq, Eq)]
enum Request {
    Ping,
    Origin,
    Enroll {
        id: String,
        branch: String,
        key: String,
    },
    Reserve {
        id: String,
        branch: String,
    },
    Revoke {
        id: String,
    },
}

fn identifier(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Request {
    fn parse(frame: &str) -> Result<Self> {
        let body = frame
            .strip_prefix(HEADER)
            .ok_or("invalid registrar protocol")?;
        let body = body
            .strip_suffix('\n')
            .ok_or("incomplete registrar request")?;
        let words: Vec<_> = body.split(' ').collect();
        let (id, branch, key) = match words.as_slice() {
            ["ping"] => return Ok(Self::Ping),
            ["origin"] => return Ok(Self::Origin),
            ["enroll", id, branch, key] => (*id, Some(*branch), Some(*key)),
            ["reserve", id, branch] => (*id, Some(*branch), None),
            ["revoke", id] => (*id, None, None),
            _ => return Err("invalid registrar request".into()),
        };
        if !identifier(id) {
            return Err("invalid instance id".into());
        }
        if let Some(branch) = branch {
            if branch.is_empty()
                || branch.len() > 200
                || !branch
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b))
            {
                return Err("invalid branch argument".into());
            }
            // The registry command enforces the complete branch grammar.
            if let Some(key) = key {
                // Ordinary Ed25519 blobs are 51 bytes: 68 base64 bytes, no padding.
                if key.len() != 68
                    || !key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"+/".contains(&b))
                {
                    return Err("invalid public key argument".into());
                }
                return Ok(Self::Enroll {
                    id: id.into(),
                    branch: branch.into(),
                    key: key.into(),
                });
            }
            return Ok(Self::Reserve {
                id: id.into(),
                branch: branch.into(),
            });
        }
        Ok(Self::Revoke { id: id.into() })
    }
}

fn receive(stream: &mut UnixStream, deadline: Instant) -> Result<String> {
    let mut bytes = Vec::with_capacity(MAX_FRAME);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("registrar read deadline")?;
        stream.set_read_timeout(Some(remaining))?;
        let mut chunk = [0u8; 128];
        let count = match stream.read(&mut chunk) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if count == 0 {
            return Ok(String::from_utf8(bytes)?);
        }
        if bytes.len().saturating_add(count) > MAX_FRAME {
            return Err("registrar frame exceeds 512 bytes".into());
        }
        bytes.extend_from_slice(chunk.get(..count).ok_or("invalid read length")?);
    }
}

fn send(stream: &mut UnixStream, frame: &str) -> Result<()> {
    if frame.len() > MAX_FRAME {
        return Err("registrar frame exceeds 512 bytes".into());
    }
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut bytes = frame.as_bytes();
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(
            deadline
                .checked_duration_since(Instant::now())
                .ok_or("registrar write deadline")?,
        ))?;
        let count = match stream.write(bytes) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if count == 0 {
            return Err("registrar write closed".into());
        }
        bytes = bytes.get(count..).ok_or("invalid write length")?;
    }
    stream.shutdown(Shutdown::Write)?;
    Ok(())
}

fn absolute(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .to_str()
            .is_none_or(|s| s.chars().any(char::is_control))
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err("registrar paths must be absolute without dot or control components".into());
    }
    Ok(())
}

fn trusted_ancestors(path: &Path, uid: u32) -> Result<()> {
    for ancestor in path.ancestors() {
        let meta = fs::symlink_metadata(ancestor)?;
        if !meta.is_dir()
            || ![0, uid].contains(&meta.uid())
            || (meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0)
        {
            return Err("untrusted registrar path ancestor".into());
        }
    }
    Ok(())
}

fn private_policy(path: &Path, uid: u32) -> Result<()> {
    absolute(path)?;
    let parent = path.parent().ok_or("missing registry parent")?;
    trusted_ancestors(parent, uid)?;
    let dir = fs::symlink_metadata(parent)?;
    let file = fs::symlink_metadata(path)?;
    if dir.uid() != uid
        || dir.mode() & 0o077 != 0
        || !file.is_file()
        || file.uid() != uid
        || file.mode() & 0o077 != 0
        || file.nlink() != 1
    {
        return Err("registry must be private and owned by the registrar account".into());
    }
    Ok(())
}

struct Endpoint {
    _listener: UnixListener,
    _lock: File,
    socket: PathBuf,
    identity: (u64, u64),
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.socket).is_ok_and(|m| (m.dev(), m.ino()) == self.identity) {
            let _ = fs::remove_file(&self.socket);
        }
    }
}

fn bind(directory: &Path, uid: u32) -> Result<Endpoint> {
    absolute(directory)?;
    trusted_ancestors(directory.parent().ok_or("missing socket ancestor")?, uid)?;
    match DirBuilder::new().mode(0o711).create(directory) {
        Ok(()) => fs::set_permissions(directory, fs::Permissions::from_mode(0o711))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let meta = fs::symlink_metadata(directory)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o7777 != 0o711 {
        return Err("registrar socket directory must be caller-owned mode 0711".into());
    }
    let lock_path = directory.join("registrar.lock");
    let lock = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock_path)
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !fs::symlink_metadata(&lock_path)?.is_file() {
                return Err("registrar lock is not a regular file".into());
            }
            OpenOptions::new().read(true).write(true).open(&lock_path)?
        }
        Err(error) => return Err(error.into()),
    };
    let meta = lock.metadata()?;
    let named = fs::symlink_metadata(&lock_path)?;
    if !named.is_file()
        || meta.uid() != uid
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
        || (meta.dev(), meta.ino()) != (named.dev(), named.ino())
    {
        return Err("untrusted registrar lock".into());
    }
    lock.try_lock()?;
    let socket = directory.join("control");
    match fs::symlink_metadata(&socket) {
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == uid => fs::remove_file(&socket)?,
        Ok(_) => return Err("refusing to replace non-registrar socket entry".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(&socket)?;
    let meta = fs::symlink_metadata(&socket)?;
    let endpoint = Endpoint {
        _listener: listener,
        _lock: lock,
        socket,
        identity: (meta.dev(), meta.ino()),
    };
    fs::set_permissions(&endpoint.socket, fs::Permissions::from_mode(0o666))?;
    Ok(endpoint)
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn execute(policy: &Path, dispatcher: &Path, request: Request) -> Result<String> {
    let mut command = Command::new(dispatcher);
    command
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    let mut scratch = None;
    let inspect_origin = matches!(request, Request::Origin);
    match request {
        Request::Ping => {
            command.arg("check").arg(policy);
        }
        Request::Origin => {
            command.arg("origin").arg(policy).stdout(Stdio::piped());
        }
        Request::Enroll { id, branch, key } => {
            let parent = policy.parent().ok_or("missing registry parent")?;
            let directory = parent.join(format!(
                "registrar-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            DirBuilder::new().mode(0o700).create(&directory)?;
            scratch = Some(Scratch(directory.clone()));
            let key_path = directory.join("public-key");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&key_path)?;
            writeln!(file, "ssh-ed25519 {key}")?;
            command
                .arg("enroll")
                .arg(policy)
                .args([id, branch])
                .arg(key_path);
        }
        Request::Reserve { id, branch } => {
            command.arg("reserve").arg(policy).args([id, branch]);
        }
        Request::Revoke { id } => {
            command.arg("revoke").arg(policy).arg(id);
        }
    }
    let mut child = command.spawn()?;
    let mut output = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let read = stdout.take(MAX_FRAME as u64 + 1).read_to_end(&mut output);
        if read.is_err() || output.len() > MAX_FRAME {
            let _ = child.kill();
            let _ = child.wait();
            return Err("invalid registry output".into());
        }
    }
    let status = child.wait()?;
    drop(scratch);
    if !status.success() {
        return Err("registry operation failed; inspect registrar diagnostics".into());
    }
    if inspect_origin {
        let origin = origin::Origin::parse(std::str::from_utf8(&output)?)?;
        Ok(format!("OK {}", origin.encode()))
    } else {
        Ok("OK\n".into())
    }
}

fn serve(directory: &Path, policy: &Path, dispatcher: &Path, operator: u32) -> Result<()> {
    let uid = fs::metadata("/proc/self")?.uid();
    private_policy(policy, uid)?;
    absolute(dispatcher)?;
    trusted_ancestors(dispatcher.parent().ok_or("missing dispatcher parent")?, uid)?;
    let meta = fs::symlink_metadata(dispatcher)?;
    if !meta.is_file()
        || ![0, uid].contains(&meta.uid())
        || meta.mode() & 0o022 != 0
        || meta.mode() & 0o111 == 0
    {
        return Err("dispatcher must be a trusted executable file".into());
    }
    execute(policy, dispatcher, Request::Ping)?;
    let endpoint = bind(directory, uid)?;
    for stream in endpoint._listener.incoming() {
        let mut stream = match stream {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
                ) =>
            {
                continue
            }
            other => other?,
        };
        if sys::peer_uid(&stream).ok() != Some(operator) {
            continue;
        }
        let result = receive(&mut stream, Instant::now() + IO_TIMEOUT)
            .and_then(|frame| Request::parse(&frame))
            .and_then(|request| {
                private_policy(policy, uid)?;
                execute(policy, dispatcher, request)
            });
        let reply = match result {
            Ok(reply) => format!("{HEADER}{reply}"),
            Err(error) => {
                eprintln!("td-vm-registrar: {error}");
                format!("{HEADER}ERROR request failed; inspect registrar diagnostics\n")
            }
        };
        let _ = send(&mut stream, &reply);
    }
    Ok(())
}

fn request(directory: &Path, server: u32, words: &[String]) -> Result<()> {
    absolute(directory)?;
    let frame = format!("{HEADER}{}\n", words.join(" "));
    let inspect_origin = matches!(Request::parse(&frame)?, Request::Origin);
    let mut stream = UnixStream::connect(directory.join("control"))?;
    if sys::peer_uid(&stream)? != server {
        return Err("registrar server account mismatch".into());
    }
    send(&mut stream, &frame)?;
    let response = receive(&mut stream, Instant::now() + Duration::from_secs(30))?;
    if inspect_origin {
        let payload = response
            .strip_prefix(&format!("{HEADER}OK "))
            .ok_or("registrar origin request failed; inspect server diagnostics")?;
        let origin = origin::Origin::parse(payload)?;
        io::stdout().lock().write_all(origin.encode().as_bytes())?;
        return Ok(());
    }
    if response != format!("{HEADER}OK\n") {
        return Err(
            "registrar request failed or its outcome is unknown; inspect server diagnostics".into(),
        );
    }
    Ok(())
}

fn run() -> Result<()> {
    let args: Vec<String> = env::args_os()
        .skip(1)
        .map(|arg| arg.into_string().map_err(|_| "arguments must be UTF-8"))
        .collect::<std::result::Result<_, _>>()?;
    match args.as_slice() {
        [verb, directory, policy, dispatcher, operator] if verb == "serve" => serve(Path::new(directory), Path::new(policy), Path::new(dispatcher), operator.parse()?),
        [verb, directory, server, remaining @ ..] if verb == "request" => request(Path::new(directory), server.parse()?, remaining),
        _ => Err("usage: td-vm-registrar serve SOCKET_DIRECTORY POLICY DISPATCHER OPERATOR_UID | request SOCKET_DIRECTORY SERVER_UID ping|origin|enroll ID BRANCH KEY_BASE64|reserve ID BRANCH|revoke ID".into()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("td-vm-registrar: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_grammar_refuses_paths_options_and_extra_frames() {
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            Request::parse(&format!("{HEADER}ping\n")).ok(),
            Some(Request::Ping)
        );
        assert!(Request::parse(&format!("{HEADER}reserve {id} topic/subtask\n")).is_ok());
        for body in [
            "ping\nrevoke anything",
            "ping extra",
            "revoke ../policy",
            "exec /bin/sh",
            "enroll id branch ssh-rsa",
            "reserve 0123456789abcdef0123456789abcdef topic;sh",
            "reserve 0123456789abcdef0123456789abcdef topic\n",
        ] {
            assert!(Request::parse(&format!("{HEADER}{body}\n")).is_err());
        }
        assert!(Request::parse("TDVM-REGISTRAR-2\nping\n").is_err());
        assert!(Request::parse(&format!("{HEADER}ping")).is_err());
    }

    #[test]
    fn framing_requires_eof_and_bounds_silent_and_oversized_peers() -> Result<()> {
        let (mut a, mut b) = UnixStream::pair()?;
        send(&mut a, &format!("{HEADER}ping\n"))?;
        assert_eq!(
            receive(&mut b, Instant::now() + IO_TIMEOUT)?,
            format!("{HEADER}ping\n")
        );
        let (_held, mut reader) = UnixStream::pair()?;
        let start = Instant::now();
        assert!(receive(&mut reader, start + Duration::from_millis(20)).is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
        let (mut writer, mut reader) = UnixStream::pair()?;
        writer.write_all(&vec![b'x'; MAX_FRAME + 1])?;
        assert!(receive(&mut reader, Instant::now() + IO_TIMEOUT).is_err());
        Ok(())
    }

    #[test]
    fn account_syscall_surface_is_confined() {
        let raw = include_str!("../vm_registrar_sys.rs");
        let root = include_str!("td-vm-registrar.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or("");
        let digest = raw.bytes().fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
        assert_eq!(digest, RAW_FINGERPRINT);
        assert_eq!(raw.matches("unsafe").count(), 2);
        assert_eq!(root.matches("unsafe").count(), 1);
        assert_eq!(root.matches("mod ").count(), 2);
        assert_eq!(root.matches("#[path").count(), 2);
        assert_eq!(root.matches("sys::").count(), 2);
        assert_eq!(root.matches("sys::peer_uid(&stream)").count(), 2);
        for text in [root, raw] {
            assert!(!text.contains("cfg_attr"));
            assert!(!text.contains("include!("));
        }
        assert!(root.contains("#[path = \"../vm_registrar_sys.rs\"]\nmod sys;"));
        assert!(root.contains("#[path = \"../vm_git_origin.rs\"]\nmod origin;"));
        for source in [include_str!("td-vm.rs"), include_str!("td-vm-git.rs")] {
            assert!(source.contains("#![forbid(unsafe_code)]"));
        }
        assert!(include_str!("../vm_git_origin.rs").contains("#![forbid(unsafe_code)]"));
        assert!(include_str!("../../Cargo.toml").contains("unsafe_code = \"deny\""));
    }
    const RAW_FINGERPRINT: u64 = 0x50343090757487b4;
}
