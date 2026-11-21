//! Session-owned website operations. Only the trusted controller may select
//! an account or authorize enrollment. The HID side supplies neither.
use crate::{
    hid::{Authenticator, Command, Report},
    Error, Result, Vault,
};
use std::time::{Duration, Instant};
use td_fido::web_request::Request;

pub enum Action<'a> {
    ChooseDevice,
    Register(&'a str, Option<&'a str>),
    Sign(&'a str),
    CheckHandle(&'a str),
}
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum VaultAction {
    UseExisting,
    EnrollAcceptingLossOfOriginalTpm,
}

#[derive(Default)]
pub struct Session {
    hid: Authenticator,
    pending: Option<Command>,
    verified: Option<u64>,
    unlocked: Option<(Vault, Instant)>,
}

pub(super) fn status(error: &Error) -> u8 {
    match error {
        Error::Missing => 0x2e,
        Error::Excluded => 0x19,
        Error::Full => 0x28,
        Error::Refused | Error::Locked => 0x27,
        Error::Stale => 0x2d,
        _ => 0x7f,
    }
}

impl Session {
    pub fn receive(
        &mut self,
        report: &Report,
        now: Instant,
        mut send: impl FnMut(&Report) -> Result<()>,
    ) -> Result<()> {
        let incoming = self.hid.receive(report, now, &mut send)?;
        self.expire(now);
        if let Some(cmd) = incoming {
            let cached = match (cmd.request()?, self.unlocked.as_ref()) {
                (Request::Get(r), Some((vault, _))) if !r.presence() => vault.preflight(&r).ok(),
                _ => None,
            };
            if let Some(reply) = cached {
                self.hid.finish(cmd.ticket(), 0, &reply, send)?;
            } else {
                self.pending = Some(cmd);
            }
        }
        Ok(())
    }
    pub fn poll(&mut self, now: Instant, send: impl FnMut(&Report) -> Result<()>) -> Result<()> {
        self.hid.poll(now, send)?;
        self.expire(now);
        Ok(())
    }
    fn expire(&mut self, now: Instant) {
        if self
            .pending
            .as_ref()
            .is_some_and(|c| !self.hid.active(c.ticket()))
        {
            self.pending = None;
        }
        if self
            .unlocked
            .as_ref()
            .is_some_and(|(_, until)| now >= *until)
        {
            self.unlocked = None;
        }
    }
    pub fn describe(&self) -> Option<(u64, Action<'_>)> {
        let c = self
            .pending
            .as_ref()
            .filter(|c| self.hid.active(c.ticket()))?;
        let action = match c.request().ok()? {
            Request::Select => Action::ChooseDevice,
            Request::Create(r) => Action::Register(r.rp(), r.user_name()),
            Request::Get(r) if r.presence() => Action::Sign(r.rp()),
            Request::Get(r) => Action::CheckHandle(r.rp()),
            Request::Info => return None,
        };
        Some((c.ticket(), action))
    }
    pub fn active(&self, ticket: u64) -> bool {
        self.hid.active(ticket)
    }
    fn checked(&self, ticket: u64) -> Result<&Command> {
        self.pending
            .as_ref()
            .filter(|c| c.ticket() == ticket && self.hid.active(ticket))
            .ok_or(Error::Stale)
    }
    /// An unlocked vault can answer a known handle probe until it is revoked.
    /// Signing and registration take a fresh trusted approval for the ticket.
    pub(super) fn begin_open(&mut self, ticket: u64, choice: VaultAction) -> Result<Instant> {
        let cmd = self.checked(ticket)?;
        if choice == VaultAction::EnrollAcceptingLossOfOriginalTpm
            && !matches!(cmd.request()?, Request::Create(_))
        {
            return Err(Error::Refused);
        }
        let deadline = cmd.deadline();
        self.verified = None;
        self.unlocked = None;
        Ok(deadline)
    }
    pub(super) fn opened(&mut self, ticket: u64, vault: Vault) -> Result<()> {
        self.checked(ticket)?;
        self.verified = Some(ticket);
        self.cache(vault, Instant::now() + Duration::from_secs(180));
        Ok(())
    }
    pub(super) fn cache(&mut self, vault: Vault, until: Instant) {
        self.unlocked = (Instant::now() < until).then_some((vault, until));
    }
    pub(super) fn verified(&self, ticket: u64) -> bool {
        self.verified == Some(ticket) && self.active(ticket)
    }
    pub fn accounts(
        &self,
        ticket: u64,
        mut offer: impl FnMut(crate::Account<'_>) -> Result<()>,
    ) -> Result<()> {
        if !self.verified(ticket) {
            return Err(Error::Refused);
        }
        let Request::Get(r) = self.checked(ticket)?.request()? else {
            return Err(Error::Invalid);
        };
        let (v, until) = self.unlocked.as_ref().ok_or(Error::Locked)?;
        if Instant::now() >= *until {
            return Err(Error::Locked);
        }
        v.list(|a| {
            if a.rp == r.rp()
                && (r.allow().ids().contains(&a.id.as_slice())
                    || (a.discoverable && r.allow().ids().is_empty()))
            {
                offer(a)?;
            }
            Ok(())
        })
    }
    pub(super) fn begin_result(&mut self, ticket: u64) -> Result<(Command, Vault, Instant)> {
        self.checked(ticket)?;
        if self.verified.take() != Some(ticket) {
            return Err(Error::Refused);
        }
        let (vault, until) = self.unlocked.take().ok_or(Error::Locked)?;
        if Instant::now() >= until {
            return Err(Error::Locked);
        }
        Ok((self.pending.take().ok_or(Error::Stale)?, vault, until))
    }
    pub fn take_physical(&mut self, ticket: u64) -> Result<Command> {
        self.checked(ticket)?;
        self.pending.take().ok_or(Error::Stale)
    }
    pub fn finish(
        &mut self,
        ticket: u64,
        code: u8,
        body: &[u8],
        send: impl FnMut(&Report) -> Result<()>,
    ) -> Result<()> {
        self.hid.finish(ticket, code, body, send)?;
        self.pending = None;
        Ok(())
    }
    pub fn lock(&mut self, send: impl FnMut(&Report) -> Result<()>) -> Result<()> {
        self.verified = None;
        self.unlocked = None;
        if let Some(command) = self.pending.take() {
            if self.hid.active(command.ticket()) {
                self.hid.finish(command.ticket(), 0x2d, &[], send)?;
            }
        }
        Ok(())
    }
}
