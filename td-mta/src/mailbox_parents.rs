//! Incremental parent-chain validation over one caller-owned read view.
use crate::{
    format::{self, key::Key, row::Row},
    ids::MailboxId,
    ports::{self, ReadView, ViewIdentity},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    View(ports::Error),
    Format(format::Error),
    ChangedView,
    Missing(MailboxId),
    Cycle,
    ReadLimit,
    Failed,
    Incomplete,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mailbox parent validation: {self:?}")
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::View(e) => Some(e),
            Self::Format(e) => Some(e),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct ParentWalk {
    identity: ViewIdentity,
    start: MailboxId,
    current: MailboxId,
    checkpoint: MailboxId,
    power: u64,
    length: u64,
    reads: u64,
    max_reads: u64,
    complete: bool,
    failed: bool,
}
impl ParentWalk {
    pub fn new(identity: ViewIdentity, start: MailboxId, max_reads: u64) -> Result<Self, Error> {
        if max_reads == 0 {
            return Err(Error::ReadLimit);
        }
        Ok(Self {
            identity,
            start,
            current: start,
            checkpoint: start,
            power: 1,
            length: 0,
            reads: 0,
            max_reads,
            complete: false,
            failed: false,
        })
    }
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    pub const fn is_complete(&self) -> bool {
        self.complete && !self.failed
    }
    pub const fn reads(&self) -> u64 {
        self.reads
    }
    /// At most one complete lookup. The view owns pins, scratch admission and
    /// lookup deadlines; the caller also checks its deadline between advances.
    pub fn advance<V: ReadView + ?Sized>(
        &mut self,
        view: &mut V,
        value: &mut [u8],
    ) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        self.failed = true;
        if view.identity() != self.identity {
            return Err(Error::ChangedView);
        }
        if self.complete {
            self.failed = false;
            return Ok(());
        }
        if self.reads >= self.max_reads {
            return Err(Error::ReadLimit);
        }
        let id = self.current;
        self.reads = self.reads.checked_add(1).ok_or(Error::ReadLimit)?;
        let found = view.get(Key::Mailbox(id), value);
        if view.identity() != self.identity {
            return Err(Error::ChangedView);
        }
        let (row, sequence) = found.map_err(Error::View)?.ok_or(Error::Missing(id))?;
        if sequence > self.identity.committed_sequence {
            return Err(Error::Format(format::Error::InvalidValue));
        }
        row.validate_key(Key::Mailbox(id)).map_err(Error::Format)?;
        let Row::Mailbox(mailbox) = row else {
            return Err(Error::Format(format::Error::InvalidValue));
        };
        let Some(parent) = mailbox.parent else {
            self.complete = true;
            self.failed = false;
            return Ok(());
        };
        self.current = parent;
        self.length = self.length.checked_add(1).ok_or(Error::ReadLimit)?;
        if self.current == self.checkpoint {
            return Err(Error::Cycle);
        }
        if self.length == self.power {
            self.checkpoint = self.current;
            self.power = self.power.checked_mul(2).ok_or(Error::ReadLimit)?;
            self.length = 0;
        }
        self.failed = false;
        Ok(())
    }
    pub fn finish(self) -> Result<CompleteChain, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        if !self.complete {
            return Err(Error::Incomplete);
        }
        Ok(CompleteChain {
            identity: self.identity,
            start: self.start,
            reads: self.reads,
        })
    }
}
/// Validation result for the supplied view; this owns no runtime pin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteChain {
    identity: ViewIdentity,
    start: MailboxId,
    reads: u64,
}
impl CompleteChain {
    pub const fn identity(self) -> ViewIdentity {
        self.identity
    }
    pub const fn start(self) -> MailboxId {
        self.start
    }
    pub const fn reads(self) -> u64 {
        self.reads
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{
        format::{row::MailboxRow, ObjectType, Sequence, Table},
        ids::{AccountId, StoreEpoch},
        ports::{ChangeCursor, ChangeStep, Record},
    };
    fn id(index: u8) -> MailboxId {
        MailboxId::from_bytes([index; 16])
    }
    fn identity() -> ViewIdentity {
        ViewIdentity {
            account: AccountId::from_bytes([1; 16]),
            epoch: StoreEpoch::from_bytes([2; 16]),
            generation: 1,
            checkpoint: Sequence::from_u64(3),
            segment: 1,
            committed_offset: 256,
            committed_sequence: Sequence::from_u64(5),
            history_floor: Sequence::from_u64(0),
        }
    }
    struct View<'a> {
        identity: ViewIdentity,
        parents: &'a [Option<u8>],
        calls: u64,
        error: Option<ports::Error>,
        change_during_get: bool,
        wrong_row: bool,
        sequence: Sequence,
    }
    impl<'a> View<'a> {
        fn new(parents: &'a [Option<u8>]) -> Self {
            Self {
                identity: identity(),
                parents,
                calls: 0,
                error: None,
                change_during_get: false,
                wrong_row: false,
                sequence: Sequence::from_u64(5),
            }
        }
    }
    impl ReadView for View<'_> {
        fn identity(&self) -> ViewIdentity {
            self.identity
        }
        fn next_change(
            &mut self,
            _: ChangeCursor,
            _: ObjectType,
        ) -> Result<ChangeStep, ports::Error> {
            Err(ports::Error::Invalid)
        }
        fn next<'a>(
            &mut self,
            _: Table,
            _: Option<&[u8]>,
            _: &'a mut [u8],
            _: &'a mut [u8],
        ) -> Result<Option<Record<'a>>, ports::Error> {
            Err(ports::Error::Invalid)
        }
        fn get<'a>(
            &mut self,
            key: Key<'_>,
            value: &'a mut [u8],
        ) -> Result<Option<(Row<'a>, Sequence)>, ports::Error> {
            self.calls += 1;
            if self.change_during_get {
                self.identity.generation += 1;
            }
            if let Some(e) = self.error {
                return Err(e);
            }
            let Key::Mailbox(mailbox) = key else {
                return Err(ports::Error::Invalid);
            };
            let index = usize::from(*mailbox.as_bytes().first().unwrap());
            let Some(parent) = self.parents.get(index) else {
                return Ok(None);
            };
            let row = if self.wrong_row {
                Row::Thread
            } else {
                Row::Mailbox(MailboxRow {
                    name: "folder",
                    parent: parent.map(id),
                    role: None,
                    sort_order: 0,
                    subscribed: true,
                })
            };
            let used = row.encode(value).map_err(|_| ports::Error::Capacity)?;
            Ok(Some((
                Row::decode(row.table(), value.get(..used).unwrap()).unwrap(),
                self.sequence,
            )))
        }
    }
    fn run(parents: &[Option<u8>], start: u8, budget: u64) -> Result<CompleteChain, Error> {
        let mut view = View::new(parents);
        let mut value = [0; 64];
        let mut walk = ParentWalk::new(view.identity(), id(start), budget)?;
        while !walk.is_complete() {
            let before = view.calls;
            walk.advance(&mut view, &mut value)?;
            assert_eq!(view.calls, before + 1);
        }
        let count = view.calls;
        walk.advance(&mut view, &mut value)?;
        assert_eq!(view.calls, count);
        let done = walk.finish()?;
        assert_eq!(done.identity(), view.identity());
        assert_eq!(done.start(), id(start));
        assert_eq!(done.reads(), view.calls);
        Ok(done)
    }
    #[test]
    fn roots_long_chains_and_cycles_have_bounded_progress() {
        assert_eq!(run(&[None], 0, 1).unwrap().reads(), 1);
        let mut parents: Vec<_> = (1..=127).map(Some).collect();
        parents.push(None);
        assert_eq!(run(&parents, 0, 128).unwrap().reads(), 128);
        assert_eq!(run(&parents, 0, 127), Err(Error::ReadLimit));
        assert_eq!(run(&parents, 96, 32).unwrap().reads(), 32);
        *parents.last_mut().unwrap() = Some(96);
        assert_eq!(run(&parents, 0, 3 * 128), Err(Error::Cycle));
        let ring: Vec<_> = (0..31).map(|n| Some((n + 1) % 31)).collect();
        assert_eq!(run(&ring, 0, 3 * 31), Err(Error::Cycle));
        assert_eq!(run(&[Some(1), Some(0)], 0, 20), Err(Error::Cycle));
        assert_eq!(
            run(&[Some(1), Some(2), Some(3), Some(1)], 0, 20),
            Err(Error::Cycle)
        );
        assert_eq!(
            run(&[Some(0)], 0, 2),
            Err(Error::Format(format::Error::InvalidValue))
        );
        assert_eq!(run(&[Some(1)], 0, 20), Err(Error::Missing(id(1))));
        assert_eq!(run(&[Some(1), None], 0, 1), Err(Error::ReadLimit));
        assert_eq!(run(&[None], 0, 0), Err(Error::ReadLimit));
        assert!(std::mem::size_of::<ParentWalk>() <= 256);
    }
    #[test]
    fn every_small_functional_graph_matches_an_independent_visited_set() {
        use std::collections::BTreeSet;
        for count in 1..=4u8 {
            let base = u32::from(count) + 2;
            for mut encoded in 0..base.pow(u32::from(count)) {
                let mut parents = Vec::new();
                for _ in 0..count {
                    let digit = encoded % base;
                    encoded /= base;
                    parents.push(if digit == 0 {
                        None
                    } else {
                        Some((digit - 1) as u8)
                    });
                }
                for start in 0..count {
                    let mut visited = BTreeSet::new();
                    let mut current = start;
                    let expected = loop {
                        if !visited.insert(current) {
                            break Err(None);
                        }
                        match parents.get(usize::from(current)) {
                            None => break Err(Some(current)),
                            Some(None) => break Ok(()),
                            Some(Some(parent)) => current = *parent,
                        }
                    };
                    let actual = run(&parents, start, 3 * u64::from(count));
                    match expected {
                        Ok(()) => assert_eq!(actual.unwrap().reads(), visited.len() as u64),
                        Err(Some(missing)) => assert_eq!(actual, Err(Error::Missing(id(missing)))),
                        Err(None) => {
                            let error = if parents.get(usize::from(current)) == Some(&Some(current))
                            {
                                Error::Format(format::Error::InvalidValue)
                            } else {
                                Error::Cycle
                            };
                            assert_eq!(actual, Err(error), "{parents:?} {start}");
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn view_errors_identity_changes_and_bad_rows_retire_the_walk() {
        let parents = [Some(1), None];
        for case in 0..8 {
            let mut view = View::new(&parents);
            let captured = view.identity();
            let expected = match case {
                0 => {
                    view.identity.generation += 1;
                    Error::ChangedView
                }
                1 => {
                    view.change_during_get = true;
                    Error::ChangedView
                }
                2 => {
                    view.error = Some(ports::Error::Deadline);
                    Error::View(ports::Error::Deadline)
                }
                3 => {
                    view.wrong_row = true;
                    Error::Format(format::Error::InvalidValue)
                }
                4 => {
                    view.sequence = Sequence::from_u64(6);
                    Error::Format(format::Error::InvalidValue)
                }
                5 => Error::View(ports::Error::Capacity),
                6 => {
                    view.change_during_get = true;
                    Error::ChangedView
                }
                _ => {
                    view.change_during_get = true;
                    view.error = Some(ports::Error::Deadline);
                    Error::ChangedView
                }
            };
            let mut walk =
                ParentWalk::new(captured, id(if case == 6 { 9 } else { 0 }), 10).unwrap();
            let mut value = [0; 64];
            let scratch = if case == 5 {
                &mut [][..]
            } else {
                &mut value[..]
            };
            assert_eq!(walk.advance(&mut view, scratch), Err(expected));
            assert!(walk.is_failed());
            let calls = view.calls;
            assert_eq!(walk.advance(&mut view, &mut value), Err(Error::Failed));
            assert_eq!(view.calls, calls);
            assert_eq!(walk.finish(), Err(Error::Failed));
        }
        let mut view = View::new(&parents);
        let mut walk = ParentWalk::new(view.identity(), id(0), 1).unwrap();
        let mut value = [0; 64];
        walk.advance(&mut view, &mut value).unwrap();
        assert_eq!(walk.reads(), 1);
        assert_eq!(walk.advance(&mut view, &mut value), Err(Error::ReadLimit));
        assert_eq!(view.calls, 1);
        assert!(walk.is_failed());
        assert_eq!(walk.finish(), Err(Error::Failed));
        assert_eq!(
            ParentWalk::new(view.identity(), id(0), 5).unwrap().finish(),
            Err(Error::Incomplete)
        );
        let mut view = View::new(&[None]);
        let mut walk = ParentWalk::new(view.identity(), id(0), 1).unwrap();
        walk.advance(&mut view, &mut value).unwrap();
        assert!(walk.is_complete());
        view.identity.generation += 1;
        assert_eq!(walk.advance(&mut view, &mut value), Err(Error::ChangedView));
        assert_eq!(view.calls, 1);
        assert!(walk.is_failed());
        assert!(!walk.is_complete());
        assert_eq!(walk.finish(), Err(Error::Failed));
    }
}
