//! Local vault management on the worker's private control socket.
use super::{Error, Result, Vault};
use crate::worker::{storage, VaultJob, VaultReply};
use std::time::{Duration, Instant};
use td_protector::pin::Pin;

pub(super) struct Manager {
    ticket: u64,
    until: Instant,
    vault: Option<Vault>,
    shown: Option<[u8; 32]>,
    job: Option<VaultJob>,
    failed: bool,
    closed: bool,
    deleted: bool,
    empty: bool,
}

impl Manager {
    pub fn open() -> Result<Self> {
        let mut serial = [0; 8];
        td_primitives::random::fill(&mut serial)?;
        let ticket = u64::from_be_bytes(serial);
        if ticket == 0 {
            return Err(Error::Refused);
        }
        Ok(Self {
            ticket,
            until: Instant::now() + Duration::from_secs(120),
            vault: None,
            shown: None,
            job: None,
            failed: false,
            closed: false,
            deleted: false,
            empty: match std::fs::symlink_metadata("/var/lib/td-passkeys/passkeys") {
                Ok(_) => false,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(e) => return Err(e.into()),
            },
        })
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.vault = None;
        self.shown = None;
        if let Some(job) = &self.job {
            job.cancel.cancel();
        }
    }

    pub fn status(&self, reply: &mut Vec<u8>) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        td_alloc::extend_from_slice(reply, &self.ticket.to_be_bytes())?;
        let state = if self.empty {
            16
        } else if self.failed {
            14
        } else if self.job.is_some() {
            12
        } else if self.deleted {
            15
        } else if self.vault.is_some() {
            13
        } else {
            11
        };
        Ok(td_alloc::push(reply, state)?)
    }

    pub fn advance(&mut self) -> bool {
        if Instant::now() >= self.until {
            self.close();
        }
        if self.job.as_ref().is_some_and(|j| j.thread.is_finished()) {
            if let Some(j) = self.job.take() {
                let result = crate::worker::vault_result(j.thread);
                if !self.closed {
                    match result {
                        Ok(VaultReply::Open(v)) if !self.deleted => self.vault = Some(v),
                        Ok(VaultReply::Open(_)) => (),
                        _ => self.failed = true,
                    }
                }
            }
        }
        self.closed && self.job.is_none()
    }

    pub fn command(&mut self, ticket: u64, op: u8, body: &[u8], reply: &mut Vec<u8>) -> Result<()> {
        if ticket != self.ticket || self.closed || Instant::now() >= self.until {
            return Err(Error::Stale);
        }
        if op == 15 && body.is_empty() {
            self.close();
            return Ok(());
        }
        if self.empty || self.failed || self.job.is_some() || self.deleted {
            return Err(Error::Locked);
        }
        match (op, body) {
            (12, pin) if self.vault.is_none() => {
                super::worker::protected()?;
                let pin = Pin::parse(pin).map_err(|_| Error::Invalid)?;
                self.job = Some(VaultJob::start(ticket, self.until, move |current| {
                    let (dir, name) = storage()?;
                    if !current() {
                        return Err(Error::Stale);
                    }
                    let tpm = td_tpm::Client::new(td_tpm::Device::open_io()?);
                    Ok(VaultReply::Open(Vault::unlock(&dir, tpm, &name, &pin)?))
                })?);
            }
            (13, [index]) => {
                let mut i = 0usize;
                self.shown = None;
                self.vault.as_ref().ok_or(Error::Locked)?.list(|a| {
                    if i == usize::from(*index) {
                        td_alloc::extend_from_slice(reply, a.id)?;
                        crate::put(reply, a.rp.as_bytes())?;
                        crate::put(reply, a.name.as_bytes())?;
                        self.shown = Some(*a.id);
                    }
                    i += 1;
                    Ok(())
                })?;
                if self.shown.is_some() {
                    td_alloc::push(
                        reply,
                        u8::try_from(i.checked_sub(1).ok_or(Error::Invalid)?)
                            .map_err(|_| Error::Full)?,
                    )?;
                }
            }
            (14, id) => {
                let id: [u8; 32] = id.try_into().map_err(|_| Error::Invalid)?;
                if self.shown != Some(id) {
                    return Err(Error::Stale);
                }
                let mut vault = self.vault.take().ok_or(Error::Locked)?;
                self.shown = None;
                self.job = Some(VaultJob::start(ticket, self.until, move |current| {
                    vault.delete_if(&id, current)?;
                    Ok(VaultReply::Open(vault))
                })?);
                self.deleted = true;
            }
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }
}
