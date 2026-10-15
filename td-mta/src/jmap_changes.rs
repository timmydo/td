//! Transaction-aligned change pages from one retained account view.
use crate::{
    format::{Sequence, MAX_TRANSACTION_OPERATIONS},
    ports::{self, Change, ChangeAction, ChangeCursor, ChangeStep, ReadView, ViewIdentity},
    store_fs::IndexReadView,
    sync::{DataState, DataType},
};

pub const MAX_CHANGES: usize = 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Store(ports::Error),
    Invalid,
    CannotCalculateChanges,
    Incomplete,
}
impl std::fmt::Display for Error {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "JMAP change-page failure: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<ports::Error> for Error {
    fn from(value: ports::Error) -> Self {
        Self::Store(value)
    }
}
/// Passive complete-page metadata; callers still own authorization and output.
#[derive(Debug)]
pub struct Page {
    pub identity: ViewIdentity,
    pub old_state: DataState,
    pub new_state: DataState,
    pub has_more: bool,
    pub changes: Vec<Change>,
}
/// A bounded history source pinned to one account snapshot. Implementations
/// check the original deadline and terminal snapshot failures on every poll.
pub trait History {
    fn history_identity(&self) -> ViewIdentity;
    fn check_history(&mut self) -> Result<(), ports::Error>;
    fn read_change(
        &mut self,
        after: ChangeCursor,
        kind: crate::format::ObjectType,
    ) -> Result<ChangeStep, ports::Error>;
}
impl History for IndexReadView<'_, '_> {
    fn history_identity(&self) -> ViewIdentity {
        ReadView::identity(self)
    }
    fn check_history(&mut self) -> Result<(), ports::Error> {
        self.check_snapshot()
    }
    fn read_change(
        &mut self,
        after: ChangeCursor,
        kind: crate::format::ObjectType,
    ) -> Result<ChangeStep, ports::Error> {
        ReadView::next_change(self, after, kind)
    }
}

/// One poll reads at most one history record. The service must charge each
/// poll against its original request work meter; no worker is started here.
pub struct Cursor<'v, V: History> {
    view: &'v mut V,
    old_state: DataState,
    identity: ViewIdentity,
    maximum: usize,
    after: ChangeCursor,
    group: Option<Sequence>,
    accepted: Sequence,
    tentative: Vec<Change>,
    checkpoint: Vec<Change>,
    complete: bool,
    more: bool,
    failed: Option<Error>,
}
impl<'v, V: History> Cursor<'v, V> {
    pub fn new(
        view: &'v mut V,
        kind: DataType,
        since: DataState,
        maximum: usize,
    ) -> Result<Self, Error> {
        view.check_history()?;
        if maximum == 0 || maximum > MAX_CHANGES {
            return Err(Error::Invalid);
        }
        let identity = view.history_identity();
        let current = DataState {
            account: identity.account,
            epoch: identity.epoch,
            kind,
            sequence: identity.committed_sequence,
        };
        if !since.is_retained_for(current, identity.history_floor) {
            return Err(Error::CannotCalculateChanges);
        }
        let mut tentative = Vec::new();
        tentative
            .try_reserve_exact(maximum + MAX_TRANSACTION_OPERATIONS)
            .map_err(|_| Error::Store(ports::Error::Capacity))?;
        let mut checkpoint = Vec::new();
        checkpoint
            .try_reserve_exact(maximum)
            .map_err(|_| Error::Store(ports::Error::Capacity))?;
        view.check_history()?;
        Ok(Self {
            view,
            old_state: since,
            identity,
            maximum,
            after: ChangeCursor {
                sequence: since.sequence,
                operation: u32::MAX,
            },
            group: None,
            accepted: since.sequence,
            tentative,
            checkpoint,
            complete: false,
            more: false,
            failed: None,
        })
    }
    pub fn poll(&mut self) -> Result<bool, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.step();
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn step(&mut self) -> Result<bool, Error> {
        self.view.check_history()?;
        if self.complete {
            return Ok(true);
        }
        match self
            .view
            .read_change(self.after, self.old_state.kind.object_type())?
        {
            ChangeStep::Record(record) => {
                if (record.cursor.sequence, record.cursor.operation)
                    <= (self.after.sequence, self.after.operation)
                    || record.cursor.sequence > self.identity.committed_sequence
                    || record.change.kind != self.old_state.kind.object_type()
                {
                    return Err(Error::Store(ports::Error::Corrupt));
                }
                if self.group != Some(record.cursor.sequence) {
                    if self.close_group()? {
                        return Ok(true);
                    }
                    self.group = Some(record.cursor.sequence);
                }
                self.after = record.cursor;
                self.merge(record.change)?;
            }
            ChangeStep::Advanced { through } => {
                if through < self.after.sequence || through > self.identity.committed_sequence {
                    return Err(Error::Store(ports::Error::Corrupt));
                }
                if self.close_group()? {
                    return Ok(true);
                }
                self.after = ChangeCursor {
                    sequence: through,
                    operation: u32::MAX,
                };
                self.accepted = through;
            }
            ChangeStep::Complete => {
                if !self.close_group()? {
                    self.accepted = self.identity.committed_sequence;
                    self.complete = true;
                }
            }
        }
        self.view.check_history()?;
        Ok(self.complete)
    }
    fn close_group(&mut self) -> Result<bool, Error> {
        let Some(group) = self.group.take() else {
            return Ok(false);
        };
        if self.tentative.len() > self.maximum {
            if self.accepted == self.old_state.sequence {
                return Err(Error::CannotCalculateChanges);
            }
            self.tentative.clear();
            self.tentative.extend_from_slice(&self.checkpoint);
            self.more = true;
            self.complete = true;
            self.view.check_history()?;
            return Ok(true);
        }
        self.accepted = group;
        self.checkpoint.clear();
        self.checkpoint.extend_from_slice(&self.tentative);
        self.view.check_history()?;
        Ok(false)
    }
    fn merge(&mut self, change: Change) -> Result<(), Error> {
        use ChangeAction::{Created, Destroyed, Updated};
        match self
            .tentative
            .binary_search_by_key(&change.id, |entry| entry.id)
        {
            Ok(index) => {
                let previous = self
                    .tentative
                    .get(index)
                    .ok_or(Error::Store(ports::Error::Corrupt))?
                    .action;
                match (previous, change.action) {
                    (Created, Destroyed) => {
                        self.tentative.remove(index);
                    }
                    (Created, Updated) | (Updated, Updated) => (),
                    (Updated, Destroyed) => {
                        self.tentative
                            .get_mut(index)
                            .ok_or(Error::Store(ports::Error::Corrupt))?
                            .action = Destroyed;
                    }
                    _ => return Err(Error::Store(ports::Error::Corrupt)),
                }
            }
            Err(index) => {
                if self.tentative.len() == self.maximum + MAX_TRANSACTION_OPERATIONS {
                    return Err(Error::Store(ports::Error::Corrupt));
                }
                self.tentative.insert(index, change);
            }
        }
        Ok(())
    }
    pub fn finish(mut self) -> Result<Page, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.view.check_history()?;
        if !self.complete {
            return Err(Error::Incomplete);
        }
        Ok(Page {
            identity: self.identity,
            old_state: self.old_state,
            new_state: DataState {
                sequence: self.accepted,
                ..self.old_state
            },
            has_more: self.more,
            changes: std::mem::take(&mut self.tentative),
        })
    }
}

#[cfg(test)]
#[path = "jmap_changes_tests.rs"]
mod tests;
