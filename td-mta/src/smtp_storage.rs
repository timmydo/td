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
    /// Copy one decoded line (at most 1000 bytes) into admitted memory.
    /// Called on the network loop: no database, filesystem, peer I/O or waits.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Failure>;
    fn prepare(&mut self, deadline: Deadline) -> Result<(), Failure>;
    fn commit(&mut self) -> Result<Completion, Failure>;
}
/// Construction admits fixed capacity; no method may wait for peer input.
/// Jobs retain original work and authority through publication or discard.
pub trait Storage: Sync {
    type Job<'a>: Job
    where
        Self: 'a;
    fn capacity(&self) -> u64;
    fn ready(&self, account: AccountId, deadline: Deadline) -> Result<(), Error>;
    fn stopped(&self) -> bool;
    fn begin(
        &self,
        input: Begin<'_, '_>,
        random: &mut td_crypto::SystemEntropy,
    ) -> Result<Self::Job<'_>, Failure>;
}

/// Complete account set for one all-or-nothing accepted DATA publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Commits {
    entries: [Option<Commit>; crate::config::identities::MAX_ACCOUNTS],
}
impl Commits {
    pub fn new(commits: impl IntoIterator<Item = Commit>) -> Result<Self, Error> {
        let mut result = Self {
            entries: [None; crate::config::identities::MAX_ACCOUNTS],
        };
        let mut count = 0;
        for commit in commits {
            if result
                .iter()
                .any(|prior| prior.account == commit.account || prior.epoch != commit.epoch)
            {
                return Err(Error::Invalid);
            }
            *result.entries.get_mut(count).ok_or(Error::Capacity)? = Some(commit);
            count += 1;
        }
        if count == 0 {
            return Err(Error::Invalid);
        }
        Ok(result)
    }
    pub fn iter(&self) -> impl Iterator<Item = &Commit> {
        self.entries.iter().flatten()
    }
}
