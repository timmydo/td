//! One queued local build and one physically confirmed installation.

use crate::consent::{self, Operation as Description, Request};
use crate::secret_sys as sys;
use crate::unlock::Event;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{chown, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SOCKET: &str = "/run/td-authd/1000/install";
const GREETING: &[u8; 8] = b"TDUPD01\n";
const LIMIT: usize = 64 + 4096;
const NOFOLLOW: i32 = 0x20000;
const NONBLOCK: i32 = 0x800;
const PATH_ONLY: i32 = 0x200000;
const ADMITTED: u8 = 2;

fn error(why: impl std::fmt::Display) -> io::Error {
    io::Error::other(why.to_string())
}
fn expires(seconds: u64) -> io::Result<Instant> {
    Instant::now()
        .checked_add(Duration::from_secs(seconds))
        .ok_or_else(|| error("update deadline overflow"))
}
fn normalized(path: &str) -> io::Result<PathBuf> {
    let mut out = PathBuf::new();
    if path.len() > 4096 || !Path::new(path).is_absolute() || path.contains('\0') {
        return Err(error("update source must be a bounded absolute path"));
    }
    for part in Path::new(path).components() {
        match part {
            Component::RootDir | Component::Normal(_) => out.push(part),
            Component::CurDir => (),
            _ => return Err(error("parent traversal is not an update source")),
        }
    }
    Ok(out)
}

pub(crate) struct Ready {
    source: File,
    deployment: String,
}
impl Ready {
    fn capture(bytes: &[u8], owner: u32) -> io::Result<Self> {
        let deployment =
            std::str::from_utf8(bytes.get(..64).ok_or_else(|| error("missing update ID"))?)
                .map_err(error)?;
        if !consent::deployment_id(deployment) {
            return Err(error("invalid update ID"));
        }
        let path = std::str::from_utf8(
            bytes
                .get(64..)
                .ok_or_else(|| error("missing update source"))?,
        )
        .map_err(error)?;
        let source = OpenOptions::new()
            .read(true)
            .custom_flags(PATH_ONLY | NOFOLLOW)
            .open(normalized(path)?)?;
        let metadata = source.metadata()?;
        if !metadata.is_dir() || metadata.uid() != owner {
            return Err(error(
                "update source must be a directory owned by its requester",
            ));
        }
        let manifest = OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW | NONBLOCK)
            .open(format!("/proc/self/fd/{}/manifest", source.as_raw_fd()))?;
        let metadata = manifest.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != owner
            || metadata.len() == 0
            || metadata.len() > 4096
        {
            return Err(error(
                "update manifest must be a bounded requester-owned file",
            ));
        }
        let mut bytes = Vec::new();
        manifest.take(4097).read_to_end(&mut bytes)?;
        if bytes.len() > 4096 || crate::sha256::hex_digest(&bytes) != deployment {
            return Err(error("update manifest does not match the requested ID"));
        }
        Ok(Self {
            source,
            deployment: deployment.into(),
        })
    }
}

struct Peer {
    pidfd: File,
    credentials: sys::Credentials,
    device: u64,
    inode: u64,
}
struct Pending {
    stream: UnixStream,
    owner: u32,
    greeting: usize,
    bytes: Vec<u8>,
    peer: Option<Peer>,
    ready: Option<Ready>,
    acknowledged: bool,
    selected: bool,
    deadline: Option<Instant>,
}
impl Pending {
    fn new(stream: UnixStream, owner: u32) -> io::Result<Self> {
        if sys::peer_uid(&stream)? != owner {
            return Err(error("update requester has the wrong UID"));
        }
        stream.set_nonblocking(true)?;
        sys::prepare_receiver(&stream)?;
        Ok(Self {
            stream,
            owner,
            greeting: 0,
            bytes: Vec::with_capacity(LIMIT + 2),
            peer: None,
            ready: None,
            acknowledged: false,
            selected: false,
            deadline: Some(expires(5)?),
        })
    }
    fn sender(&mut self, sender: sys::Sender) -> io::Result<()> {
        if sender.credentials.uid != self.owner || sender.descriptor.is_some() {
            return Err(error("update request has a foreign sender or descriptor"));
        }
        sys::alive(sender.pidfd.as_fd())?;
        let pidfd = File::from(sender.pidfd);
        let metadata = pidfd.metadata()?;
        if let Some(peer) = &self.peer {
            if sender.credentials != peer.credentials
                || metadata.dev() != peer.device
                || metadata.ino() != peer.inode
            {
                return Err(error("update sender changed"));
            }
            sys::alive(peer.pidfd.as_fd())?;
        } else {
            self.peer = Some(Peer {
                pidfd,
                credentials: sender.credentials,
                device: metadata.dev(),
                inode: metadata.ino(),
            });
        }
        Ok(())
    }
    fn live(&self) -> io::Result<()> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(error("update request expired"));
        }
        if let Some(peer) = &self.peer {
            sys::alive(peer.pidfd.as_fd())?;
        }
        Ok(())
    }
    fn poll(&mut self) -> io::Result<()> {
        self.live()?;
        for _ in 0..4 {
            if self.greeting < GREETING.len() {
                match self.stream.write(
                    GREETING
                        .get(self.greeting..)
                        .ok_or_else(|| error("update greeting cursor"))?,
                ) {
                    Ok(0) => return Err(error("update requester disconnected")),
                    Ok(count) => self.greeting += count,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    Err(e) => return Err(e),
                }
                continue;
            }
            if self.ready.is_some() {
                if !self.acknowledged {
                    match self.stream.write(&[ADMITTED]) {
                        Ok(1) => {
                            self.acknowledged = true;
                            self.deadline = Some(expires(60)?);
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                        _ => return Err(error("update admission reply failed")),
                    }
                    continue;
                }
                let mut byte = [0];
                return match sys::receive(&self.stream, &mut byte) {
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                        ) =>
                    {
                        Ok(())
                    }
                    _ => Err(error("update requester disconnected or sent extra bytes")),
                };
            }
            let expected = if let Some(header) = self.bytes.get(..2) {
                let length = usize::from(u16::from_be_bytes(header.try_into().map_err(error)?));
                if !(65..=LIMIT).contains(&length) {
                    return Err(error("invalid update request length"));
                }
                length + 2
            } else {
                2
            };
            if self.bytes.len() == expected && expected > 2 {
                self.ready = Some(Ready::capture(
                    self.bytes
                        .get(2..)
                        .ok_or_else(|| error("missing update request"))?,
                    self.owner,
                )?);
                return self.live();
            }
            let mut bytes = [0; LIMIT + 2];
            let remaining = expected
                .checked_sub(self.bytes.len())
                .ok_or_else(|| error("invalid update cursor"))?;
            let bytes = bytes
                .get_mut(..remaining)
                .ok_or_else(|| error("update frame overflow"))?;
            match sys::receive(&self.stream, bytes) {
                Ok((count, sender)) => {
                    self.sender(sender)?;
                    self.bytes.extend_from_slice(
                        bytes
                            .get(..count)
                            .ok_or_else(|| error("update receive count"))?,
                    );
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
        self.live()
    }
}

pub(crate) struct Intake {
    listener: UnixListener,
    owner: u32,
    pending: Option<Pending>,
    in_flight: bool,
    identity: (u64, u64),
}
impl Intake {
    /// Credential preparation has already admitted these root-owned parents.
    pub fn bind(owner: u32) -> Result<Self, String> {
        if owner != 1000 {
            return Err("unsupported update requester".into());
        }
        for path in ["/run", "/run/td-authd", "/run/td-authd/1000"] {
            let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
            if !metadata.is_dir()
                || metadata.uid() != 0
                || metadata.gid() != 0
                || metadata.mode() & 0o7022 != 0
            {
                return Err("update intake parent is not protected".into());
            }
        }
        match fs::symlink_metadata(SOCKET) {
            Ok(metadata)
                if metadata.file_type().is_socket() && matches!(metadata.uid(), 0 | 1000) =>
            {
                fs::remove_file(SOCKET).map_err(|e| e.to_string())?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            _ => return Err("unexpected update intake endpoint".into()),
        }
        let listener = UnixListener::bind(SOCKET).map_err(|e| e.to_string())?;
        let metadata = fs::symlink_metadata(SOCKET).map_err(|e| e.to_string())?;
        let intake = Self {
            listener,
            owner,
            pending: None,
            in_flight: false,
            identity: (metadata.dev(), metadata.ino()),
        };
        fs::set_permissions(SOCKET, fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        chown(SOCKET, Some(owner), Some(owner)).map_err(|e| e.to_string())?;
        intake
            .listener
            .set_nonblocking(true)
            .map_err(|e| e.to_string())?;
        Ok(intake)
    }
    pub fn tick(&mut self) {
        if let Ok((stream, _)) = self.listener.accept() {
            if self.pending.is_none() && !self.in_flight {
                self.pending = Pending::new(stream, self.owner).ok();
            }
        }
        if self
            .pending
            .as_mut()
            .is_some_and(|pending| pending.poll().is_err())
        {
            self.pending = None;
        }
    }
    pub fn select(&mut self) -> Result<Ready, String> {
        self.tick();
        let pending = self.pending.as_mut().ok_or("no pending update")?;
        if !pending.acknowledged || pending.selected {
            return Err("update is not selectable".into());
        }
        pending.live().map_err(|e| e.to_string())?;
        let ready = pending.ready.as_ref().ok_or("incomplete update request")?;
        let captured = Ready {
            source: ready.source.try_clone().map_err(|e| e.to_string())?,
            deployment: ready.deployment.clone(),
        };
        pending.selected = true;
        pending.deadline = Some(expires(120).map_err(|e| e.to_string())?);
        self.in_flight = true;
        Ok(captured)
    }
    pub fn selected_alive(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.selected && pending.live().is_ok())
    }
    pub fn committed(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.deadline = None;
        }
    }
    pub fn finish(&mut self, success: bool) {
        self.in_flight = false;
        if let Some(mut pending) = self.pending.take() {
            let _ = pending.stream.write(&[u8::from(success)]);
        }
    }
}
impl Drop for Intake {
    fn drop(&mut self) {
        if fs::symlink_metadata(SOCKET).is_ok_and(|metadata| {
            metadata.file_type().is_socket() && (metadata.dev(), metadata.ino()) == self.identity
        }) {
            let _ = fs::remove_file(SOCKET);
        }
    }
}

pub(crate) fn request(source: &str, deployment: &str) -> Result<(), String> {
    let source = normalized(source).map_err(|e| e.to_string())?;
    if !consent::deployment_id(deployment) {
        return Err("invalid deployment ID".into());
    }
    let source = source.to_str().ok_or("update source is not UTF-8")?;
    let mut stream = UnixStream::connect(SOCKET)
        .map_err(|e| format!("connect to installation authority: {e}"))?;
    if sys::peer_uid(&stream).map_err(|e| e.to_string())? != 0 {
        return Err("installation authority is not root".into());
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut greeting = [0; 8];
    stream
        .read_exact(&mut greeting)
        .map_err(|e| format!("installation admission unavailable (busy or disconnected): {e}"))?;
    if &greeting != GREETING {
        return Err("unsupported installation authority".into());
    }
    let mut bytes = Vec::with_capacity(LIMIT + 2);
    let length = u16::try_from(deployment.len() + source.len()).map_err(|e| e.to_string())?;
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(deployment.as_bytes());
    bytes.extend_from_slice(source.as_bytes());
    stream.write_all(&bytes).map_err(|e| e.to_string())?;
    let mut reply = [0];
    stream
        .read_exact(&mut reply)
        .map_err(|e| format!("update admission failed: {e}"))?;
    if reply != [ADMITTED] {
        return Err("update was not admitted".into());
    }
    writeln!(
        io::stdout(),
        "Update {deployment} is ready. Press Ctrl+Alt+Escape, then I to review it."
    )
    .map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3600)))
        .map_err(|e| e.to_string())?;
    stream
        .read_exact(&mut reply)
        .map_err(|e| format!("installation ended without a completion receipt: {e}"))?;
    if reply == [1] {
        writeln!(io::stdout(), "System installed. Restart to boot it.").map_err(|e| e.to_string())
    } else {
        Err("system installation was cancelled or failed".into())
    }
}

pub(crate) struct Installation {
    request: Request,
    source: Option<File>,
    presented: bool,
    committed: bool,
    deadline: Instant,
    child: Option<Child>,
    result: Option<bool>,
}
impl Installation {
    pub fn start(owner: u32, ready: Ready) -> Result<Self, String> {
        if owner != 1000 {
            return Err("unsupported installation owner".into());
        }
        let mut nonce = [0; 32];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut nonce))
            .map_err(|e| e.to_string())?;
        let request = Request::new(
            nonce,
            owner,
            Description::Install {
                deployment: ready.deployment,
                requester: owner,
            },
        )?;
        Ok(Self {
            request,
            source: Some(ready.source),
            presented: false,
            committed: false,
            deadline: expires(120).map_err(|e| e.to_string())?,
            child: None,
            result: None,
        })
    }
    pub fn request(&self) -> &Request {
        &self.request
    }
    fn admit(&self, request: &Request) -> Result<(), String> {
        if request != &self.request
            || self.result.is_some()
            || self.committed
            || Instant::now() >= self.deadline
        {
            return Err("stale installation consent".into());
        }
        Ok(())
    }
    pub fn presented(&mut self, request: &Request) -> Result<(), String> {
        self.admit(request)?;
        if self.presented {
            return Err("duplicate installation presentation".into());
        }
        self.presented = true;
        Ok(())
    }
    pub fn commit(&mut self, request: &Request) -> Result<(), String> {
        self.commit_with(request, |source, deployment| {
            Command::new("/bin/td-update")
                .args(["apply-operation", deployment])
                .env_clear()
                .current_dir("/")
                .stdin(Stdio::from(source))
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
        })
    }
    fn commit_with(
        &mut self,
        request: &Request,
        spawn: impl FnOnce(File, &str) -> io::Result<Child>,
    ) -> Result<(), String> {
        self.admit(request)?;
        if !self.presented {
            return Err("installation was not presented".into());
        }
        let Description::Install { deployment, .. } = self.request.operation() else {
            return Err("invalid installation description".into());
        };
        let source = self.source.take().ok_or("missing approved source")?;
        self.committed = true;
        let child = spawn(source, deployment);
        match child {
            Ok(child) => self.child = Some(child),
            Err(_) => self.result = Some(false),
        }
        Ok(())
    }
    pub fn cancel(&mut self, _reason: &str) -> Result<(), String> {
        // Confirmed installation is one running transaction, not a renewable
        // approval. Closing the screen cannot undo a publication already made.
        if !self.committed {
            self.source = None;
            self.result = Some(false);
        }
        Ok(())
    }
    pub fn poll(&mut self) -> Result<Event, String> {
        if !self.committed && Instant::now() >= self.deadline {
            self.cancel("installation request expired")?;
        }
        if let Some(child) = &mut self.child {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                self.result = Some(status.success());
                self.child = None;
            }
        }
        Ok(match self.result {
            Some(true) => Event::Complete,
            Some(false) => Event::Failed("system installation failed or was cancelled".into()),
            None if self.committed => Event::Waiting,
            None if self.presented => Event::Commit(self.request.clone()),
            None => Event::Present(self.request.clone()),
        })
    }
    pub fn reap_for_teardown(mut self) -> Result<(), String> {
        if let Some(child) = &mut self.child {
            let stopped = child.kill();
            child
                .wait()
                .map_err(|e| format!("reap installation child: {e}; stop: {stopped:?}"))?;
            self.child = None;
        }
        Ok(())
    }
}
impl Drop for Installation {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
#[path = "../tests/deployment.rs"]
mod tests;
