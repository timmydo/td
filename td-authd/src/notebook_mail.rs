//! Portal-only intake for exact, remembered notebook mail bindings.

use crate::notebook_wire::{
    field, read_binding, Bytes, MailBinding, PendingWire, Reader, MAIL_LIMIT, MAIL_PENDING,
    MAIL_SOCKET,
};
use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

const PORTAL: u32 = 991;
const DEADLINE: Duration = Duration::from_secs(5);

enum Phase {
    Reading,
    Waiting(MailBinding),
    Replying,
}
struct Pending {
    id: u64,
    wire: PendingWire,
    phase: Phase,
    deadline: Instant,
}
impl Pending {
    fn new(id: u64, stream: UnixStream, owner: u32) -> Result<Self, String> {
        if crate::secret_sys::peer_uid(&stream).map_err(|_| "inspect notebook caller")? != owner {
            return Err("notebook mail caller is not the portal".into());
        }
        Ok(Self {
            id,
            wire: PendingWire::bounded(stream, MAIL_LIMIT)?,
            phase: Phase::Reading,
            deadline: Instant::now() + DEADLINE,
        })
    }
    fn poll(&mut self) -> Result<Option<MailBinding>, String> {
        if Instant::now() >= self.deadline {
            return Err("notebook mail request expired".into());
        }
        if let Some(frame) = self.wire.poll()? {
            if !matches!(self.phase, Phase::Reading) {
                return Err("repeated notebook mail request".into());
            }
            let mut input = Reader::new(&frame.0);
            if input.byte()? != 1 {
                return Err("unknown notebook mail request".into());
            }
            let binding = read_binding(&mut input)?;
            input.finish()?;
            self.phase = Phase::Waiting(binding.clone());
            return Ok(Some(binding));
        }
        if matches!(self.phase, Phase::Replying) && !self.wire.sending() {
            return Err("notebook mail response complete".into());
        }
        Ok(None)
    }
}

pub(crate) struct Intake {
    listener: UnixListener,
    identity: (u64, u64),
    pending: Vec<Pending>,
    next: u64,
    requests: Vec<MailBinding>,
}
impl Intake {
    pub fn bind() -> Result<Self, String> {
        for path in ["/run", "/run/td-authd", "/run/td-authd/1000"] {
            let metadata =
                fs::symlink_metadata(path).map_err(|_| "inspect notebook intake parent")?;
            if !metadata.is_dir()
                || metadata.uid() != 0
                || metadata.gid() != 0
                || metadata.mode() & 0o7022 != 0
            {
                return Err("notebook intake parent is not root-controlled".into());
            }
        }
        match fs::symlink_metadata(MAIL_SOCKET) {
            Ok(metadata)
                if metadata.file_type().is_socket() && matches!(metadata.uid(), 0 | PORTAL) =>
            {
                fs::remove_file(MAIL_SOCKET).map_err(|_| "retire notebook intake socket")?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return Err("unexpected notebook intake endpoint".into()),
        }
        let listener = UnixListener::bind(MAIL_SOCKET).map_err(|_| "bind notebook intake")?;
        let metadata = fs::symlink_metadata(MAIL_SOCKET).map_err(|_| "inspect notebook socket")?;
        let intake = Self {
            listener,
            identity: (metadata.dev(), metadata.ino()),
            pending: Vec::new(),
            next: 0,
            requests: Vec::new(),
        };
        fs::set_permissions(MAIL_SOCKET, fs::Permissions::from_mode(0o600))
            .map_err(|_| "protect notebook intake")?;
        std::os::unix::fs::chown(MAIL_SOCKET, Some(PORTAL), Some(PORTAL))
            .map_err(|_| "assign notebook intake")?;
        intake
            .listener
            .set_nonblocking(true)
            .map_err(|_| "configure notebook intake")?;
        Ok(intake)
    }
    pub fn requests(&self) -> &[MailBinding] {
        &self.requests
    }
    pub fn dismiss(&mut self, binding: &MailBinding) {
        self.requests.retain(|request| request != binding);
    }
    pub fn revoke(&mut self) {
        for pending in &self.pending {
            pending.wire.close();
        }
        self.pending.clear();
    }
    pub fn poll(&mut self) -> Option<(u64, MailBinding)> {
        if let Ok((stream, _)) = self.listener.accept() {
            if self.pending.len() < MAIL_PENDING {
                if let Some(id) = self.next.checked_add(1) {
                    self.next = id;
                    if let Ok(pending) = Pending::new(id, stream, PORTAL) {
                        self.pending.push(pending);
                    }
                }
            }
        }
        let mut selected = None;
        self.pending.retain_mut(|pending| {
            if selected.is_some() {
                return true;
            }
            match pending.poll() {
                Ok(Some(binding)) => {
                    if selected.is_some() {
                        return false;
                    }
                    selected = Some((pending.id, binding));
                    true
                }
                Ok(None) => true,
                Err(_) => false,
            }
        });
        if let Some((_, binding)) = &selected {
            if !self.requests.contains(binding) && self.requests.len() < MAIL_PENDING {
                self.requests.push(binding.clone());
            }
        }
        selected
    }
    pub fn reply(&mut self, id: u64, reply: Bytes) {
        let mut approved = None;
        for pending in &mut self.pending {
            if pending.id != id {
                continue;
            }
            let Phase::Waiting(binding) = &pending.phase else {
                return;
            };
            if reply.0.first() == Some(&0) {
                approved = Some(binding.clone());
            }
            if pending.wire.queue(reply).is_ok() {
                pending.phase = Phase::Replying;
            } else {
                pending.deadline = Instant::now();
            }
            break;
        }
        if let Some(binding) = approved {
            self.dismiss(&binding);
        }
    }
}
impl Drop for Intake {
    fn drop(&mut self) {
        self.revoke();
        if fs::symlink_metadata(MAIL_SOCKET)
            .is_ok_and(|m| m.file_type().is_socket() && (m.dev(), m.ino()) == self.identity)
        {
            let _ = fs::remove_file(MAIL_SOCKET);
        }
    }
}

pub(crate) fn refused(reason: &str) -> Result<Bytes, String> {
    let mut response = Bytes(vec![1]);
    field(&mut response, reason.as_bytes())?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::notebook_wire::{put_binding, Wire};
    use std::io::Write;

    fn pair() -> (Pending, UnixStream) {
        let (server, client) = UnixStream::pair().unwrap();
        let uid = crate::secret_sys::peer_uid(&server).unwrap();
        (Pending::new(7, server, uid).unwrap(), client)
    }
    #[test]
    fn rejects_other_principal_before_accepting_any_request() {
        let (server, _client) = UnixStream::pair().unwrap();
        let uid = crate::secret_sys::peer_uid(&server).unwrap();
        assert!(Pending::new(1, server, uid.wrapping_add(1)).is_err());
    }
    #[test]
    fn exact_request_is_single_use_and_bounded_before_allocation() {
        let binding = MailBinding::new(
            "work".into(),
            "https://mail.invalid/jmap".into(),
            "me".into(),
        )
        .unwrap();
        let (mut pending, client) = pair();
        let mut client = Wire::new(client).unwrap();
        let mut frame = Bytes(vec![1]);
        put_binding(&mut frame, &binding).unwrap();
        client.send(&frame.0).unwrap();
        assert_eq!(pending.poll().unwrap(), Some(binding));
        client.send(&frame.0).unwrap();
        assert!(pending.poll().is_err());
        let (mut pending, mut client) = pair();
        client
            .write_all(&((MAIL_LIMIT + 1) as u32).to_be_bytes())
            .unwrap();
        assert!(pending.poll().is_err());
    }
    #[test]
    fn idle_and_waiting_requests_share_one_deadline() {
        let (mut pending, _client) = pair();
        assert!(pending.poll().unwrap().is_none());
        pending.deadline = Instant::now();
        assert!(pending.poll().is_err());
    }
}
