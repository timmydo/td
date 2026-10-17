//! Requests and outcomes shared by the PostgreSQL JMAP adapters.
pub use crate::ports::CommitFailure as CommitError;
use crate::{
    admission::logical,
    format::{self, Sequence},
    ids::{AccountId, BlobId, DeviceId, EmailId, MailboxId, StoreEpoch, ThreadId},
    ports::{self, Deadline},
};

#[derive(Debug)]
pub enum UploadError {
    Store(ports::Error),
    Ledger(logical::Error),
    Format(format::Error),
}
impl From<ports::Error> for UploadError {
    fn from(value: ports::Error) -> Self {
        Self::Store(value)
    }
}
impl From<logical::Error> for UploadError {
    fn from(value: logical::Error) -> Self {
        Self::Ledger(value)
    }
}
impl From<format::Error> for UploadError {
    fn from(value: format::Error) -> Self {
        Self::Format(value)
    }
}
impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => e.fmt(f),
            Self::Ledger(e) => e.fmt(f),
            Self::Format(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for UploadError {}

pub const MAX_EMAIL_UPDATES: usize = 256;

#[derive(Clone, Copy, Debug)]
pub struct EmailUpdate {
    /// Replace all memberships with this one existing account mailbox.
    pub mailbox: Option<MailboxId>,
    pub email: EmailId,
    pub seen: Option<bool>,
    pub flagged: Option<bool>,
}
/// Passive inputs. Authorization and account state are checked under coordination.
#[derive(Clone, Copy, Debug)]
pub struct EmailUpdateRequest<'a> {
    pub account: AccountId,
    pub device: DeviceId,
    pub expected: Option<(StoreEpoch, Sequence)>,
    pub updates: &'a [EmailUpdate],
    pub deadline: Deadline,
}
#[derive(Debug)]
#[must_use = "preserve the durable result even when admission has stopped"]
pub struct EmailUpdateCompletion {
    pub before_epoch: StoreEpoch,
    pub before_sequence: Sequence,
    pub outcome: Result<Sequence, CommitError>,
    /// In request order; refused objects have no writes or history.
    pub results: Vec<EmailUpdateResult>,
    pub admission_stopped: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmailUpdateResult {
    Updated,
    NotFound,
    InvalidMailbox,
}

pub const MAX_EMAIL_DESTROYS: usize = 256;
#[derive(Clone, Copy, Debug)]
pub struct EmailDestroyRequest<'a> {
    pub account: AccountId,
    pub device: DeviceId,
    pub expected: Option<(StoreEpoch, Sequence)>,
    pub emails: &'a [EmailId],
    pub deadline: Deadline,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmailDestroyResult {
    Destroyed,
    NotFound,
}
#[derive(Debug)]
#[must_use = "only a known commit makes the planned deletions durable"]
pub struct EmailDestroyCompletion {
    pub before_epoch: StoreEpoch,
    pub before_sequence: Sequence,
    pub outcome: Result<Sequence, CommitError>,
    /// Planned per-object outcomes; durable only when outcome is Ok.
    pub results: Vec<EmailDestroyResult>,
    pub admission_stopped: bool,
}

pub const MAX_MAILBOX_CREATES: usize = 256;

#[derive(Clone, Copy)]
pub struct MailboxCreateRequest<'a> {
    pub account: AccountId,
    pub device: DeviceId,
    pub expected: Option<(StoreEpoch, Sequence)>,
    pub names: &'a [&'a str],
    pub deadline: Deadline,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MailboxCreateResult {
    Created(MailboxId),
    InvalidName,
    AlreadyExists,
}
#[derive(Debug)]
#[must_use = "preserve the durable result even when admission has stopped"]
pub struct MailboxCreateCompletion {
    pub before_epoch: StoreEpoch,
    pub before_sequence: Sequence,
    pub outcome: Result<Sequence, CommitError>,
    /// Planned results become durable only when outcome is Ok.
    pub results: Vec<MailboxCreateResult>,
    pub admission_stopped: bool,
}

#[derive(Debug)]
#[must_use = "preserve known publication even if accounting or cleanup fails"]
pub struct DraftCompletion {
    pub before_epoch: StoreEpoch,
    pub before_sequence: Sequence,
    pub email: EmailId,
    pub blob: BlobId,
    pub thread: ThreadId,
    pub size: u64,
    pub outcome: Result<Sequence, CommitError>,
    pub admission_stopped: bool,
    pub cleanup_error: Option<ports::Error>,
}
