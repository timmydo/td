//! Admitted SMTP jobs: bounded DATA copies and fixed-worker publication.
use crate::{
    format::row::ReceiptTls,
    ids::AccountId,
    ports::{Clock, Commit, CommitFailure, Deadline, Error},
    smtp_session::Session,
};
use std::{net::IpAddr, sync::Arc};
pub fn trace_allowance(hostname: &str) -> Result<usize, Error> {
    crate::receiving_headers::trace_allowance(hostname)
}

pub struct Begin<'a, 'cfg> {
    pub config_generation: u64,
    pub peer: IpAddr,
    pub tls: ReceiptTls,
    pub clock: Arc<dyn Clock>,
    pub session: &'a Session<'cfg>,
    pub header_bytes: usize,
    pub deadline: Deadline,
}
/// One newly offered RCPT account, checked before its 250.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Recipient {
    Accepted,
    /// At its body or blob cap, or without room for the declared SIZE.
    Full,
    /// The declared SIZE exceeds the account's whole body cap.
    ExceedsQuota,
    /// No single Inbox to file into.
    Unavailable,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    /// No publication or new work was consumed; retain the original deadline.
    Retry,
    HeaderLimit,
    Refused(Error),
}
#[must_use = "a known durable outcome survives cleanup or accounting failure"]
pub struct Completion {
    pub outcome: Result<Commits, CommitFailure>,
    pub stop: bool,
}
pub trait Job: Send {
    /// Copy one decoded chunk (at most 1000 bytes) into admitted memory.
    /// A chunk may split a physical line or its CRLF.
    /// Called on the network loop: no database, filesystem, peer I/O or waits.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Failure>;
    fn prepare(&mut self, deadline: Deadline) -> Result<(), Failure>;
    /// The verdict belongs to this SMTP transaction. Backends that retain
    /// received messages record it beside the local trace.
    fn prepare_with_spf(
        &mut self,
        deadline: Deadline,
        _spf: Option<crate::spf::Outcome>,
    ) -> Result<(), Failure> {
        self.prepare(deadline)
    }
    /// Original decoded DATA while this job retains its ingress permit.
    fn dkim_input(&self) -> Option<&[u8]> {
        None
    }
    fn prepare_with_auth(
        &mut self,
        deadline: Deadline,
        spf: Option<crate::spf::Outcome>,
        _dkim: Option<crate::dkim::Report>,
    ) -> Result<(), Failure> {
        self.prepare_with_spf(deadline, spf)
    }
    fn prepare_with_dmarc(
        &mut self,
        deadline: Deadline,
        spf: Option<crate::spf::Outcome>,
        dkim: Option<crate::dkim::Report>,
        _dmarc: Option<crate::dmarc::Report>,
    ) -> Result<(), Failure> {
        self.prepare_with_auth(deadline, spf, dkim)
    }
    fn commit(&mut self) -> Result<Completion, Failure>;
}
/// Construction admits fixed capacity; no method may wait for peer input.
/// Jobs retain original work and authority through publication or discard.
pub trait Storage: Sync {
    type Job<'a>: Job
    where
        Self: 'a;
    /// Called only by the fixed storage worker, including idle scans.
    fn maintain(&self) -> Result<(), Error> {
        Ok(())
    }
    /// Retire worker-owned resources before that worker exits.
    fn close(&self) -> Result<(), Error> {
        Ok(())
    }
    fn capacity(&self) -> u64;
    fn ready(&self, account: AccountId, deadline: Deadline) -> Result<(), Error>;
    /// Check an offered RCPT's account quota and Inbox. This is admission
    /// evidence only; publication rechecks both under its account locks.
    fn recipient(
        &self,
        account: AccountId,
        declared_bytes: Option<u64>,
        deadline: Deadline,
    ) -> Result<Recipient, Failure>;
    fn stopped(&self) -> bool;
    fn begin(
        &self,
        input: Begin<'_, '_>,
        random: &mut td_crypto::SystemEntropy,
    ) -> Result<Self::Job<'_>, Failure>;
}

/// Complete account set for one accepted DATA publication. Each account
/// received the message or, when its quota refused the message after RCPT,
/// a local notice in the same transaction; at least one received it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Commits {
    entries: [Option<(Commit, bool)>; crate::config::identities::MAX_ACCOUNTS],
}
impl Commits {
    pub fn new(commits: impl IntoIterator<Item = Commit>) -> Result<Self, Error> {
        Self::with_notices(commits, [])
    }
    pub fn with_notices(
        delivered: impl IntoIterator<Item = Commit>,
        noticed: impl IntoIterator<Item = Commit>,
    ) -> Result<Self, Error> {
        let mut result = Self {
            entries: [None; crate::config::identities::MAX_ACCOUNTS],
        };
        let entries = delivered
            .into_iter()
            .map(|commit| (commit, false))
            .chain(noticed.into_iter().map(|commit| (commit, true)));
        for (count, (commit, notice)) in entries.enumerate() {
            if result
                .iter()
                .any(|prior| prior.account == commit.account || prior.epoch != commit.epoch)
            {
                return Err(Error::Invalid);
            }
            *result.entries.get_mut(count).ok_or(Error::Capacity)? = Some((commit, notice));
        }
        if result.delivered().next().is_none() {
            return Err(Error::Invalid);
        }
        Ok(result)
    }
    /// Every committed account, delivered or noticed.
    pub fn iter(&self) -> impl Iterator<Item = &Commit> {
        self.entries.iter().flatten().map(|(commit, _)| commit)
    }
    pub fn delivered(&self) -> impl Iterator<Item = &Commit> {
        self.entries
            .iter()
            .flatten()
            .filter(|(_, notice)| !notice)
            .map(|(commit, _)| commit)
    }
}
