//! Check every enumerated mailbox chain without retaining the forest.
use crate::{
    format::{self, key::Key, row::Row, Table},
    ids::MailboxId,
    mailbox_parents::{self, ParentWalk},
    ports::{self, ReadView, ViewIdentity},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    View(ports::Error),
    Format(format::Error),
    Parent(mailbox_parents::Error),
    ChangedView,
    RowLimit,
    Failed,
    Incomplete,
}
impl From<mailbox_parents::Error> for Error {
    fn from(error: mailbox_parents::Error) -> Self {
        match error {
            mailbox_parents::Error::ChangedView => Self::ChangedView,
            _ => Self::Parent(error),
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mailbox forest validation: {self:?}")
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::View(e) => Some(e),
            Self::Format(e) => Some(e),
            Self::Parent(e) => Some(e),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    Mailbox(MailboxId),
    Walking,
    Rooted { mailbox: MailboxId, reads: u64 },
    Complete,
}
pub struct Sweep {
    identity: ViewIdentity,
    prior: Option<MailboxId>,
    walk: Option<ParentWalk>,
    max_rows: u64,
    max_reads: u64,
    mailboxes: u64,
    reads: u64,
    complete: bool,
    failed: bool,
}
impl Sweep {
    pub const fn new(identity: ViewIdentity, max_rows: u64, max_reads: u64) -> Self {
        Self {
            identity,
            prior: None,
            walk: None,
            max_rows,
            max_reads,
            mailboxes: 0,
            reads: 0,
            complete: false,
            failed: false,
        }
    }
    pub const fn is_failed(&self) -> bool {
        self.failed
    }
    pub const fn is_complete(&self) -> bool {
        self.complete && !self.failed
    }
    /// One next or one get at most; caller admits the lookup's full work.
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
        let step = self.work(view, key, value)?;
        self.failed = false;
        Ok(step)
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
        if self.complete {
            return Ok(Step::Complete);
        }
        if let Some(walk) = self.walk.as_mut() {
            walk.advance(view, value).map_err(Error::from)?;
            self.reads = self
                .reads
                .checked_add(1)
                .ok_or(Error::Format(format::Error::Overflow))?;
            if !walk.is_complete() {
                return Ok(Step::Walking);
            }
            let walk = self
                .walk
                .take()
                .ok_or(Error::Incomplete)?
                .finish()
                .map_err(Error::from)?;
            self.mailboxes = self
                .mailboxes
                .checked_add(1)
                .ok_or(Error::Format(format::Error::Overflow))?;
            return Ok(Step::Rooted {
                mailbox: walk.start(),
                reads: walk.reads(),
            });
        }
        let after = self.prior.as_ref().map(|id| id.as_bytes().as_slice());
        let found = view.next(Table::Mailboxes, after, key, value);
        if view.identity() != self.identity {
            return Err(Error::ChangedView);
        }
        let Some(record) = found.map_err(Error::View)? else {
            self.complete = true;
            return Ok(Step::Complete);
        };
        let Key::Mailbox(id) = record.key else {
            return Err(Error::Format(format::Error::InvalidValue));
        };
        if self.prior.is_some_and(|prior| prior >= id)
            || record.last_change > self.identity.committed_sequence
        {
            return Err(Error::Format(format::Error::InvalidValue));
        }
        record.row.validate_key(record.key).map_err(Error::Format)?;
        if !matches!(record.row, Row::Mailbox(_)) {
            return Err(Error::Format(format::Error::InvalidValue));
        }
        if self.mailboxes >= self.max_rows {
            return Err(Error::RowLimit);
        }
        let remaining = self
            .max_reads
            .checked_sub(self.reads)
            .ok_or(Error::Parent(mailbox_parents::Error::ReadLimit))?;
        self.walk = Some(ParentWalk::new(self.identity, id, remaining).map_err(Error::from)?);
        self.prior = Some(id);
        Ok(Step::Mailbox(id))
    }
    pub fn finish(self) -> Result<CompleteForest, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        if !self.complete {
            return Err(Error::Incomplete);
        }
        Ok(CompleteForest {
            identity: self.identity,
            mailboxes: self.mailboxes,
            reads: self.reads,
        })
    }
}
/// All enumerated chains reached roots; physical completeness and pins remain external.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteForest {
    identity: ViewIdentity,
    mailboxes: u64,
    reads: u64,
}
impl CompleteForest {
    pub const fn identity(self) -> ViewIdentity {
        self.identity
    }
    pub const fn mailboxes(self) -> u64 {
        self.mailboxes
    }
    pub const fn reads(self) -> u64 {
        self.reads
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        format::{row::MailboxRow, ObjectType, Sequence},
        ids::{AccountId, StoreEpoch},
        ports::{ChangeCursor, ChangeStep, Record},
    };
    fn id(n: u8) -> MailboxId {
        MailboxId::from_bytes([n; 16])
    }
    fn identity() -> ViewIdentity {
        ViewIdentity {
            account: AccountId::from_bytes([1; 16]),
            epoch: StoreEpoch::from_bytes([2; 16]),

            committed_sequence: Sequence::from_u64(2),
            history_floor: Sequence::default(),
        }
    }
    fn mailbox(parent: Option<u8>) -> Row<'static> {
        Row::Mailbox(MailboxRow {
            name: "box",
            parent: parent.map(id),
            role: None,
            sort_order: 0,
            subscribed: true,
        })
    }
    struct View<'a> {
        identity: ViewIdentity,
        parents: &'a [Option<u8>],
        nexts: usize,
        gets: usize,
        moved: bool,
        absent: bool,
        error: bool,
        bad_row: bool,
        bad_key: bool,
        future: bool,
        forced: Option<u8>,
    }
    impl<'a> View<'a> {
        fn new(parents: &'a [Option<u8>]) -> Self {
            Self {
                identity: identity(),
                parents,
                nexts: 0,
                gets: 0,
                moved: false,
                absent: false,
                error: false,
                bad_row: false,
                bad_key: false,
                future: false,
                forced: None,
            }
        }
        fn fault(&mut self) -> Result<bool, ports::Error> {
            if self.moved {
                self.identity.epoch = crate::ids::StoreEpoch::from_bytes([99; 16]);
            }
            if self.error {
                return Err(ports::Error::Capacity);
            }
            Ok(self.absent)
        }
        fn row<'b>(
            &self,
            parent: Option<u8>,
            value: &'b mut [u8],
        ) -> Result<(Row<'b>, Sequence), ports::Error> {
            let row = if self.bad_row {
                Row::Thread
            } else {
                mailbox(parent)
            };
            let len = row.encode(value).map_err(|_| ports::Error::Capacity)?;
            Ok((
                Row::decode(row.table(), &value[..len]).unwrap(),
                Sequence::from_u64(if self.future { 3 } else { 2 }),
            ))
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
        fn get<'a>(
            &mut self,
            key: Key<'_>,
            value: &'a mut [u8],
        ) -> Result<Option<(Row<'a>, Sequence)>, ports::Error> {
            self.gets += 1;
            if self.fault()? {
                return Ok(None);
            }
            let Key::Mailbox(id) = key else {
                return Err(ports::Error::Invalid);
            };
            let Some(&parent) = self.parents.get(usize::from(id.as_bytes()[0])) else {
                return Ok(None);
            };
            self.row(parent, value).map(Some)
        }
        fn next<'a>(
            &mut self,
            table: Table,
            after: Option<&[u8]>,
            key: &'a mut [u8],
            value: &'a mut [u8],
        ) -> Result<Option<Record<'a>>, ports::Error> {
            assert_eq!(table, Table::Mailboxes);
            self.nexts += 1;
            if self.fault()? {
                return Ok(None);
            }
            for (index, &parent) in self.parents.iter().enumerate() {
                let identifier = id(u8::try_from(index).unwrap());
                if let Some(forced) = self.forced {
                    if usize::from(forced) != index {
                        continue;
                    }
                } else if after.is_some_and(|prior| prior >= identifier.as_bytes()) {
                    continue;
                }
                let len = Key::Mailbox(identifier).encode(key).unwrap();
                let source = if self.bad_key {
                    Key::Thread(crate::ids::ThreadId::from_bytes([1; 16]))
                } else {
                    Key::decode(table, &key[..len]).unwrap()
                };
                let (row, last_change) = self.row(parent, value)?;
                return Ok(Some(Record {
                    key: source,
                    row,
                    last_change,
                }));
            }
            Ok(None)
        }
    }
    pub(crate) fn probe() {
        for (parents, reads) in [(&[None, Some(0), Some(1), None][..], 7), (&[][..], 0)] {
            let mut view = View::new(parents);
            let mut sweep = Sweep::new(identity(), parents.len() as u64, reads);
            let mut rooted = 0;
            let mut started = 0;
            for _ in 0..20 {
                let before = view.nexts + view.gets;
                let step = sweep
                    .advance(&mut view, &mut [0; 16], &mut [0; 128])
                    .unwrap();
                assert_eq!(view.nexts + view.gets, before + 1);
                match step {
                    Step::Mailbox(_) => started += 1,
                    Step::Rooted { mailbox, reads } => {
                        let expected = [(id(0), 1), (id(1), 2), (id(2), 3), (id(3), 1)];
                        assert_eq!(expected.get(rooted), Some(&(mailbox, reads)));
                        rooted += 1;
                    }
                    Step::Walking => {}
                    Step::Complete => break,
                }
            }
            assert!(sweep.is_complete());
            assert_eq!((started, rooted), (parents.len(), parents.len()));
            assert_eq!(view.nexts, parents.len() + 1);
            assert_eq!(view.gets as u64, reads);
            assert_eq!(
                sweep.advance(&mut view, &mut [], &mut []).unwrap(),
                Step::Complete
            );
            assert_eq!(view.nexts, parents.len() + 1);
            assert_eq!(view.gets as u64, reads);
            let complete = sweep.finish().unwrap();
            assert_eq!(complete.identity(), identity());
            assert_eq!(complete.mailboxes(), parents.len() as u64);
            assert_eq!(complete.reads(), reads);
        }
    }
    #[test]
    fn every_chain_is_walked_and_empty_views_need_no_budget() {
        probe();
        assert!(std::mem::size_of::<Sweep>() <= 512);
    }
    #[test]
    fn cycles_missing_parents_and_separate_total_budgets_refuse() {
        type Case<'a> = (&'a [Option<u8>], u64, u64, Error);
        let cases: [Case<'_>; 5] = [
            (
                &[None, Some(2), Some(1)],
                3,
                20,
                Error::Parent(mailbox_parents::Error::Cycle),
            ),
            (
                &[None, Some(5)],
                2,
                10,
                Error::Parent(mailbox_parents::Error::Missing(id(5))),
            ),
            (&[None, Some(0)], 1, 10, Error::RowLimit),
            (
                &[None, Some(0)],
                2,
                2,
                Error::Parent(mailbox_parents::Error::ReadLimit),
            ),
            (
                &[None],
                1,
                0,
                Error::Parent(mailbox_parents::Error::ReadLimit),
            ),
        ];
        for (parents, rows, reads, expected) in cases {
            let mut view = View::new(parents);
            let mut sweep = Sweep::new(identity(), rows, reads);
            let mut error = None;
            for _ in 0..30 {
                if let Err(e) = sweep.advance(&mut view, &mut [0; 16], &mut [0; 128]) {
                    error = Some(e);
                    break;
                }
            }
            assert_eq!(error, Some(expected));
            assert!(view.gets as u64 <= reads);
            assert!(sweep.is_failed());
            let calls = view.nexts + view.gets;
            assert_eq!(
                sweep.advance(&mut view, &mut [], &mut []),
                Err(Error::Failed)
            );
            assert_eq!(view.nexts + view.gets, calls);
            assert!(matches!(sweep.finish(), Err(Error::Failed)));
        }
    }
    #[test]
    fn next_and_get_faults_retire_the_coordinator() {
        for during_get in [false, true] {
            for mode in 0..8 {
                let mut view = View::new(&[None]);
                let mut sweep = Sweep::new(identity(), 1, 1);
                if during_get {
                    assert_eq!(
                        sweep
                            .advance(&mut view, &mut [0; 16], &mut [0; 128])
                            .unwrap(),
                        Step::Mailbox(id(0))
                    );
                }
                let before = view.nexts + view.gets;
                match mode {
                    0 => view.identity.epoch = crate::ids::StoreEpoch::from_bytes([99; 16]),
                    1 => view.moved = true,
                    2 => {
                        view.moved = true;
                        view.absent = true;
                    }
                    3 => {
                        view.moved = true;
                        view.error = true;
                    }
                    4 => view.error = true,
                    5 => view.bad_row = true,
                    6 => view.future = true,
                    _ => {
                        if during_get {
                            view.absent = true;
                        } else {
                            view.bad_key = true;
                        }
                    }
                }
                let expected = match mode {
                    0..=3 => Error::ChangedView,
                    4 if during_get => {
                        Error::Parent(mailbox_parents::Error::View(ports::Error::Capacity))
                    }
                    4 => Error::View(ports::Error::Capacity),
                    7 if during_get => Error::Parent(mailbox_parents::Error::Missing(id(0))),
                    _ if during_get => {
                        Error::Parent(mailbox_parents::Error::Format(format::Error::InvalidValue))
                    }
                    _ => Error::Format(format::Error::InvalidValue),
                };
                assert_eq!(
                    sweep.advance(&mut view, &mut [0; 16], &mut [0; 128]),
                    Err(expected)
                );
                assert_eq!(view.nexts + view.gets, before + usize::from(mode != 0));
                assert!(matches!(sweep.finish(), Err(Error::Failed)));
            }
        }
    }
    #[test]
    fn ordering_incomplete_and_changed_completion_refuse() {
        for lower in [false, true] {
            let mut view = View::new(&[None, None]);
            view.forced = Some(1);
            let mut sweep = Sweep::new(identity(), 1, 1);
            assert_eq!(
                sweep
                    .advance(&mut view, &mut [0; 16], &mut [0; 128])
                    .unwrap(),
                Step::Mailbox(id(1))
            );
            assert!(matches!(
                sweep.advance(&mut view, &mut [], &mut [0; 128]).unwrap(),
                Step::Rooted { .. }
            ));
            if lower {
                view.forced = Some(0);
            }
            assert_eq!(
                sweep.advance(&mut view, &mut [0; 16], &mut [0; 128]),
                Err(Error::Format(format::Error::InvalidValue))
            );
            assert_eq!(view.gets, 1);
            assert!(matches!(sweep.finish(), Err(Error::Failed)));
        }
        assert!(matches!(
            Sweep::new(identity(), 0, 0).finish(),
            Err(Error::Incomplete)
        ));
        let mut view = View::new(&[]);
        let mut sweep = Sweep::new(identity(), 0, 0);
        assert_eq!(
            sweep.advance(&mut view, &mut [], &mut []).unwrap(),
            Step::Complete
        );
        view.identity.epoch = crate::ids::StoreEpoch::from_bytes([99; 16]);
        assert_eq!(
            sweep.advance(&mut view, &mut [], &mut []),
            Err(Error::ChangedView)
        );
        assert_eq!(view.nexts, 1);
        assert!(matches!(sweep.finish(), Err(Error::Failed)));
    }
}
