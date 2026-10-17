//! Storage boundary used only on the fixed SMTP worker.
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
    pub account: AccountId,
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
    pub outcome: Result<Commit, CommitFailure>,
    pub stop: bool,
}
pub trait Job: Send {
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
