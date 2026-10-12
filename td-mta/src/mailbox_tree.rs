//! Bounded retained paths for Mailbox/query tree ordering and ancestor filtering.
use crate::{
    ids::MailboxId,
    mailbox_parents::{Error, ParentWalk},
    ports::{ReadView, ViewIdentity},
};
use std::cmp::Ordering;

pub const MAX_DEPTH: usize = 32;
/// A completed path is passive snapshot metadata, not a view pin or authority.
#[derive(Clone, Debug)]
pub struct Path {
    identity: ViewIdentity,
    ids: [MailboxId; MAX_DEPTH],
    length: usize,
}
impl Path {
    pub const fn identity(&self) -> ViewIdentity {
        self.identity
    }
    pub fn from_root(&self) -> impl DoubleEndedIterator<Item = MailboxId> + ExactSizeIterator + '_ {
        self.ids.iter().take(self.length).rev().copied()
    }
    /// Compare ancestors before descendants. Otherwise return the nearest pair
    /// sharing a parent, whose mailbox fields the query's comparator must compare.
    pub fn relation(&self, other: &Self) -> Result<Relation, Error> {
        if self.identity != other.identity {
            return Err(Error::ChangedView);
        }
        let (mut left, mut right) = (self.from_root(), other.from_root());
        loop {
            match (left.next(), right.next()) {
                (None, None) => return Ok(Relation::Ordered(Ordering::Equal)),
                (None, Some(_)) => return Ok(Relation::Ordered(Ordering::Less)),
                (Some(_), None) => return Ok(Relation::Ordered(Ordering::Greater)),
                (Some(a), Some(b)) if a != b => return Ok(Relation::Siblings(a, b)),
                (Some(_), Some(_)) => {}
            }
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Relation {
    /// Ancestors always precede descendants, independent of sort direction.
    Ordered(Ordering),
    Siblings(MailboxId, MailboxId),
}
/// Retains at most 32 IDs while reusing the existing one-lookup parent validator.
/// The caller charges each step and retains its original read view/deadline.
#[derive(Debug)]
pub struct Walk {
    parents: ParentWalk,
    path: Path,
    failed: bool,
}
impl Walk {
    pub fn new(identity: ViewIdentity, start: MailboxId) -> Result<Self, Error> {
        Ok(Self {
            parents: ParentWalk::new(identity, start, MAX_DEPTH as u64)?,
            path: Path {
                identity,
                ids: [MailboxId::from_bytes([0; 16]); MAX_DEPTH],
                length: 0,
            },
            failed: false,
        })
    }
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    pub fn is_complete(&self) -> bool {
        !self.failed && self.parents.is_complete()
    }
    /// At most one lookup, including validation of the original view identity.
    /// A depth over 32 returns ReadLimit; no truncated path becomes successful.
    pub fn advance<V: ReadView + ?Sized>(
        &mut self,
        view: &mut V,
        value: &mut [u8],
    ) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        self.failed = true;
        let next = self.parents.next_id();
        self.parents.advance(view, value)?;
        if let Some(id) = next {
            *self
                .path
                .ids
                .get_mut(self.path.length)
                .ok_or(Error::ReadLimit)? = id;
            self.path.length += 1;
        }
        self.failed = false;
        Ok(())
    }
    pub fn finish(self) -> Result<Path, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        let complete = self.parents.finish()?;
        if complete.identity() != self.path.identity || complete.reads() != self.path.length as u64
        {
            return Err(Error::Failed);
        }
        Ok(self.path)
    }
}
#[cfg(test)]
#[path = "mailbox_tree_tests.rs"]
mod tests;
