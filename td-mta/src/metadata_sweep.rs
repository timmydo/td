//! Compose bounded metadata checks over one supplied final view.
use crate::{
    format::Table,
    mailbox_sweep,
    ports::{ReadView, ViewIdentity},
    recipient_sweep, reference_sweep,
};

/// Each pass may enumerate this many rows; parent gets have a separate total.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub rows: u64,
    pub parent_reads: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    References(reference_sweep::Error),
    Mailboxes(mailbox_sweep::Error),
    Recipients(recipient_sweep::Error),
    ChangedView,
    Counts,
    Failed,
    Incomplete,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "metadata checks: {self:?}")
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::References(e) => Some(e),
            Self::Mailboxes(e) => Some(e),
            Self::Recipients(e) => Some(e),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    References(reference_sweep::Step),
    Mailboxes(mailbox_sweep::Step),
    Recipients(recipient_sweep::Step),
    Complete,
}
// Serial inline children share one fixed reservation without heap owners.
#[allow(clippy::large_enum_variant)]
enum Phase {
    References(reference_sweep::Sweep),
    Mailboxes(mailbox_sweep::Sweep),
    Recipients(recipient_sweep::Sweep),
    Finalize,
    Complete,
}
pub struct Sweep {
    identity: ViewIdentity,
    limits: Limits,
    phase: Phase,
    references: Option<reference_sweep::CompleteSweep>,
    mailboxes: Option<mailbox_sweep::CompleteForest>,
    recipients: Option<recipient_sweep::CompleteCoverage>,
    failed: bool,
}
impl Sweep {
    pub const fn new(identity: ViewIdentity, utc_ms: i64, limits: Limits) -> Self {
        Self {
            identity,
            limits,
            phase: Phase::References(reference_sweep::Sweep::new(identity, utc_ms, limits.rows)),
            references: None,
            mailboxes: None,
            recipients: None,
            failed: false,
        }
    }
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    pub const fn is_complete(&self) -> bool {
        !self.failed && matches!(self.phase, Phase::Complete)
    }
    /// One child turn: at most one next and two gets; admit their full work.
    pub fn advance<V: ReadView + ?Sized>(
        &mut self,
        view: &mut V,
        key: &mut [u8],
        value: &mut [u8],
    ) -> Result<Step, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        self.failed = true;
        let result = self.work(view, key, value)?;
        self.failed = false;
        Ok(result)
    }
    fn work<V: ReadView + ?Sized>(
        &mut self,
        view: &mut V,
        key: &mut [u8],
        value: &mut [u8],
    ) -> Result<Step, Error> {
        if view.identity() != self.identity {
            return Err(Error::ChangedView);
        }
        match std::mem::replace(&mut self.phase, Phase::Complete) {
            Phase::References(mut sweep) => {
                let step = sweep.advance(view, key, value).map_err(|e| match e {
                    reference_sweep::Error::ChangedView => Error::ChangedView,
                    _ => Error::References(e),
                })?;
                if sweep.is_complete() {
                    self.references = Some(sweep.finish().map_err(Error::References)?);
                    self.phase = Phase::Mailboxes(mailbox_sweep::Sweep::new(
                        self.identity,
                        self.limits.rows,
                        self.limits.parent_reads,
                    ));
                } else {
                    self.phase = Phase::References(sweep);
                }
                Ok(Step::References(step))
            }
            Phase::Mailboxes(mut sweep) => {
                let step = sweep.advance(view, key, value).map_err(|e| match e {
                    mailbox_sweep::Error::ChangedView => Error::ChangedView,
                    _ => Error::Mailboxes(e),
                })?;
                if sweep.is_complete() {
                    self.mailboxes = Some(sweep.finish().map_err(Error::Mailboxes)?);
                    self.phase = Phase::Recipients(recipient_sweep::Sweep::new(
                        self.identity,
                        self.limits.rows,
                    ));
                } else {
                    self.phase = Phase::Mailboxes(sweep);
                }
                Ok(Step::Mailboxes(step))
            }
            Phase::Recipients(mut sweep) => {
                let step = sweep.advance(view, key, value).map_err(|e| match e {
                    recipient_sweep::Error::ChangedView => Error::ChangedView,
                    _ => Error::Recipients(e),
                })?;
                if sweep.is_complete() {
                    self.recipients = Some(sweep.finish().map_err(Error::Recipients)?);
                    self.phase = Phase::Finalize;
                } else {
                    self.phase = Phase::Recipients(sweep);
                }
                Ok(Step::Recipients(step))
            }
            Phase::Finalize => {
                let references = self.references.ok_or(Error::Incomplete)?;
                let mailboxes = self.mailboxes.ok_or(Error::Incomplete)?;
                let recipients = self.recipients.ok_or(Error::Incomplete)?;
                if references.table_rows(Table::Mailboxes) != Some(mailboxes.mailboxes())
                    || references.table_rows(Table::Submissions) != Some(recipients.submissions())
                    || references.table_rows(Table::Recipients) != Some(recipients.recipients())
                {
                    return Err(Error::Counts);
                }
                Ok(Step::Complete)
            }
            Phase::Complete => Ok(Step::Complete),
        }
    }
    pub fn finish(self) -> Result<CompleteMetadata, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        if !self.is_complete() {
            return Err(Error::Incomplete);
        }
        Ok(CompleteMetadata {
            references: self.references.ok_or(Error::Incomplete)?,
            mailboxes: self.mailboxes.ok_or(Error::Incomplete)?,
            recipients: self.recipients.ok_or(Error::Incomplete)?,
        })
    }
}
/// Supplied rows only; body integrity, physical completeness and authority stay external.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteMetadata {
    references: reference_sweep::CompleteSweep,
    mailboxes: mailbox_sweep::CompleteForest,
    recipients: recipient_sweep::CompleteCoverage,
}

impl CompleteMetadata {
    pub const fn references(self) -> reference_sweep::CompleteSweep {
        self.references
    }
    pub const fn mailboxes(self) -> mailbox_sweep::CompleteForest {
        self.mailboxes
    }
    pub const fn recipients(self) -> recipient_sweep::CompleteCoverage {
        self.recipients
    }
}

#[cfg(test)]
#[path = "metadata_sweep_tests.rs"]
mod tests;
