//! One immutable human request, selected only by the private attention peer.

use crate::consent::Operation;
use crate::secret_request::{Credential, Target, ADMITTED, GREETING, LIMIT, MAX_SECRET, SOCKET};
use crate::secret_sys as sys;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{chown, DirBuilderExt, FileExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

#[path = "../../td-firstboot/src/principals.rs"]
#[allow(dead_code, reason = "shared immutable installed-account admission")]
mod principals;

const ADMISSION_TIME: Duration = Duration::from_secs(5);
const QUEUE_TIME: Duration = Duration::from_secs(60);

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
    input: Vec<u8>,
    peer: Option<Peer>,
    descriptor: Option<File>,
    operation: Option<Operation>,
    acknowledged: bool,
    selected: bool,
    deadline: Instant,
}

impl Pending {
    fn new(stream: UnixStream, owner: u32) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        if sys::peer_uid(&stream)? != owner {
            return Err(io::Error::other(
                "credential connection has the wrong owner",
            ));
        }
        sys::prepare_receiver(&stream)?;
        Ok(Self {
            stream,
            owner,
            greeting: 0,
            input: Vec::with_capacity(LIMIT + 2),
            peer: None,
            descriptor: None,
            operation: None,
            acknowledged: false,
            selected: false,
            deadline: expires(ADMISSION_TIME)?,
        })
    }

    fn sender(&mut self, sender: sys::Sender) -> io::Result<()> {
        if sender.credentials.uid != self.owner {
            return Err(io::Error::other("credential sender has the wrong owner"));
        }
        sys::alive(sender.pidfd.as_fd())?;
        let pidfd = File::from(sender.pidfd);
        let metadata = pidfd.metadata()?;
        if let Some(peer) = &self.peer {
            if sender.credentials != peer.credentials
                || metadata.dev() != peer.device
                || metadata.ino() != peer.inode
            {
                return Err(io::Error::other("credential sender changed"));
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
        if let Some(file) = sender.descriptor {
            if self.descriptor.is_some() {
                return Err(io::Error::other("multiple credential descriptors"));
            }
            self.descriptor = Some(file);
        }
        Ok(())
    }

    fn live(&self) -> io::Result<()> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::other("credential request expired"));
        }
        if let Some(peer) = &self.peer {
            sys::alive(peer.pidfd.as_fd())?;
        }
        Ok(())
    }

    fn poll(&mut self) -> io::Result<()> {
        self.poll_with(admit_target)
    }

    fn poll_with(
        &mut self,
        admit: impl FnOnce(&Target, u32) -> io::Result<Operation>,
    ) -> io::Result<()> {
        self.live()?;
        for _ in 0..4 {
            if self.greeting < GREETING.len() {
                match self.stream.write(
                    GREETING
                        .get(self.greeting..)
                        .ok_or_else(|| io::Error::other("invalid greeting cursor"))?,
                ) {
                    Ok(0) => return Err(io::Error::other("credential requester disconnected")),
                    Ok(count) => self.greeting += count,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    Err(e) => return Err(e),
                }
                continue;
            }
            if self.operation.is_some() {
                if !self.acknowledged {
                    match self.stream.write(&[ADMITTED]) {
                        Ok(1) => {
                            self.acknowledged = true;
                            self.deadline = expires(QUEUE_TIME)?;
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                        Err(e) => return Err(e),
                        _ => return Err(io::Error::other("credential admission reply failed")),
                    }
                    continue;
                }
                // No traffic follows the one request. Reading solely detects
                // disconnect or extra data; neither can alter the snapshot.
                let mut byte = [0];
                return match sys::receive(&self.stream, &mut byte) {
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) =>
                    {
                        Ok(())
                    }
                    _ => Err(io::Error::other(
                        "credential requester closed or sent extra data",
                    )),
                };
            }
            let expected = if let Some(header) = self.input.get(..2) {
                let length = usize::from(u16::from_be_bytes(
                    header.try_into().map_err(io::Error::other)?,
                ));
                if length == 0 || length > LIMIT {
                    return Err(io::Error::other("invalid credential target length"));
                }
                length + 2
            } else {
                2
            };
            if self.input.len() == expected && expected > 2 {
                let target = Target::decode(
                    self.input
                        .get(2..)
                        .ok_or_else(|| io::Error::other("missing target"))?,
                )
                .map_err(io::Error::other)?;
                validate_descriptor(
                    self.descriptor
                        .as_ref()
                        .ok_or_else(|| io::Error::other("missing credential descriptor"))?,
                )?;
                self.operation = Some(admit(&target, self.owner)?);
                return self.live();
            }
            let mut bytes = [0; LIMIT + 2];
            let remaining = expected
                .checked_sub(self.input.len())
                .ok_or_else(|| io::Error::other("invalid target cursor"))?;
            let bytes = bytes
                .get_mut(..remaining)
                .ok_or_else(|| io::Error::other("invalid target bound"))?;
            match sys::receive(&self.stream, bytes) {
                Ok((count, sender)) => {
                    self.sender(sender)?;
                    self.input.extend_from_slice(
                        bytes
                            .get(..count)
                            .ok_or_else(|| io::Error::other("invalid target read count"))?,
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

impl Pending {
    fn capture(&mut self) -> Result<(Operation, Credential), String> {
        if !self.acknowledged {
            return Err("credential admission is not acknowledged".into());
        }
        if self.selected {
            return Err("credential request already selected".into());
        }
        self.live().map_err(|e| e.to_string())?;
        let operation = self
            .operation
            .clone()
            .ok_or("credential request is incomplete")?;
        let file = self
            .descriptor
            .as_ref()
            .ok_or("missing credential descriptor")?;
        let size = validate_descriptor(file).map_err(|e| e.to_string())?;
        let mut credential = Credential(vec![0; size]);
        file.read_exact_at(&mut credential.0, 0)
            .map_err(|e| e.to_string())?;
        self.live().map_err(|e| e.to_string())?;
        self.selected = true;
        self.deadline = expires(Duration::from_secs(120)).map_err(|e| e.to_string())?;
        Ok((operation, credential))
    }
}

fn admit_target(target: &Target, owner: u32) -> io::Result<Operation> {
    let registry = principals::Registry::load().map_err(io::Error::other)?;
    registry
        .verify_installed_accounts(&registry)
        .map_err(io::Error::other)?;
    let active = registry.active_applications().map_err(io::Error::other)?;
    let app = active
        .iter()
        .find(|app| app.owner == owner && app.name == target.app)
        .ok_or_else(|| io::Error::other("credential target is not installed"))?;
    target.operation(owner, app.uid).map_err(io::Error::other)
}

fn expires(duration: Duration) -> io::Result<Instant> {
    Instant::now()
        .checked_add(duration)
        .ok_or_else(|| io::Error::other("credential deadline overflow"))
}

fn validate_descriptor(file: &File) -> io::Result<usize> {
    sys::require_sealed(file)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_SECRET as u64 {
        return Err(io::Error::other(
            "credential must contain 1 through 4096 bytes",
        ));
    }
    Ok(metadata.len() as usize)
}

pub(crate) struct Intake {
    listener: UnixListener,
    owner: u32,
    pending: Option<Pending>,
    socket_identity: (u64, u64),
    in_flight: bool,
}

impl Intake {
    pub fn bind(owner: u32) -> Result<Self, String> {
        if owner != 1000 {
            return Err("unsupported credential intake owner".into());
        }
        for path in ["/run", "/run/td-authd", "/run/td-authd/1000"] {
            match fs::DirBuilder::new().mode(0o755).create(path) {
                Ok(()) => (),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e.to_string()),
            }
            let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
            if !metadata.is_dir()
                || metadata.uid() != 0
                || metadata.gid() != 0
                || metadata.mode() & 0o7022 != 0
            {
                return Err("credential intake parent is not protected".into());
            }
        }
        match fs::symlink_metadata(SOCKET) {
            Ok(metadata)
                if metadata.file_type().is_socket() && matches!(metadata.uid(), 0 | 1000) =>
            {
                fs::remove_file(SOCKET).map_err(|e| e.to_string())?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            _ => return Err("unexpected credential intake endpoint".into()),
        }
        let listener = UnixListener::bind(SOCKET).map_err(|e| e.to_string())?;
        let metadata = fs::symlink_metadata(SOCKET).map_err(|e| e.to_string())?;
        let intake = Self {
            listener,
            owner,
            pending: None,
            socket_identity: (metadata.dev(), metadata.ino()),
            in_flight: false,
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

    /// One accept and four I/O attempts per authority heartbeat.
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

    pub fn select(&mut self) -> Result<(Operation, Credential), String> {
        crate::secret_request::require_protected_memory()?;
        self.tick();
        let pending = self
            .pending
            .as_mut()
            .ok_or("no pending credential request")?;
        let captured = pending.capture()?;
        self.in_flight = true;
        Ok(captured)
    }

    pub fn selected_alive(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.selected && pending.live().is_ok())
    }

    pub fn finish(&mut self, success: bool) {
        self.in_flight = false;
        if let Some(mut pending) = self.pending.take() {
            let _ = pending.stream.write(&[if success { 1 } else { 0 }]);
        }
    }
}

impl Drop for Intake {
    fn drop(&mut self) {
        if fs::symlink_metadata(SOCKET).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && (metadata.dev(), metadata.ino()) == self.socket_identity
        }) {
            let _ = fs::remove_file(SOCKET);
        }
    }
}

#[cfg(test)]
#[path = "../tests/secret_intake.rs"]
mod tests;
