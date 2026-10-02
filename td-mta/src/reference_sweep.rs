//! Direct-reference enumeration over a supplied view; full graph activation stays external.
use crate::{
    format::{self, key::Key, Table, MAX_KEY_BYTES, TABLE_COUNT},
    ports::{self, ReadView, ViewIdentity},
    row_references::{self, ReferenceCheck},
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    View(ports::Error),
    Format(format::Error),
    References(row_references::Error),
    ChangedView,
    RowLimit,
    Failed,
    Incomplete,
}
impl From<row_references::Error> for Error {
    fn from(error: row_references::Error) -> Self {
        match error {
            row_references::Error::ChangedView => Self::ChangedView,
            _ => Self::References(error),
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "direct-reference sweep: {self:?}")
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::View(e) => Some(e),
            Self::Format(e) => Some(e),
            Self::References(e) => Some(e),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    Row { table: Table },
    TableComplete { table: Table },
    Complete,
}
pub struct Sweep {
    identity: ViewIdentity,
    utc_ms: i64,
    max_rows: u64,
    rows: u64,
    counts: [u64; TABLE_COUNT],
    table: usize,
    cursor: [u8; MAX_KEY_BYTES],
    cursor_len: usize,
    complete: bool,
    failed: bool,
}
impl Sweep {
    pub const fn new(identity: ViewIdentity, utc_ms: i64, max_rows: u64) -> Self {
        Self {
            identity,
            utc_ms,
            max_rows,
            rows: 0,
            counts: [0; TABLE_COUNT],
            table: 0,
            cursor: [0; MAX_KEY_BYTES],
            cursor_len: 0,
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
    /// Admit one next and at most two gets, with the view's full lookup work limits.
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
        if self.table == TABLE_COUNT {
            self.complete = true;
            return Ok(Step::Complete);
        }
        let tag = self
            .table
            .checked_add(1)
            .and_then(|n| u16::try_from(n).ok())
            .ok_or(Error::Format(format::Error::Overflow))?;
        let table = Table::from_tag(tag).map_err(Error::Format)?;
        let after = if self.cursor_len == 0 {
            None
        } else {
            Some(
                self.cursor
                    .get(..self.cursor_len)
                    .ok_or(Error::Format(format::Error::InvalidValue))?,
            )
        };
        let next = view.next(table, after, key, value);
        if view.identity() != self.identity {
            return Err(Error::ChangedView);
        }
        let Some(record) = next.map_err(Error::View)? else {
            self.table = self
                .table
                .checked_add(1)
                .ok_or(Error::Format(format::Error::Overflow))?;
            self.cursor_len = 0;
            return Ok(Step::TableComplete { table });
        };
        if record.key.table() != table {
            return Err(Error::Format(format::Error::InvalidValue));
        }
        // Copy the key to release next's shared key/value lifetime before target gets.
        let mut encoded = [0; MAX_KEY_BYTES];
        let len = record.key.encode(&mut encoded).map_err(Error::Format)?;
        let encoded = encoded
            .get(..len)
            .ok_or(Error::Format(format::Error::Limit))?;
        if after.is_some_and(|prior| prior >= encoded) {
            return Err(Error::Format(format::Error::InvalidValue));
        }
        let source = Key::decode(table, encoded).map_err(Error::Format)?;
        let mut check = ReferenceCheck::new(
            self.identity,
            source,
            record.row,
            record.last_change,
            self.utc_ms,
        )
        .map_err(Error::from)?;
        if self.rows >= self.max_rows {
            return Err(Error::RowLimit);
        }
        for _ in 0..2 {
            if check.advance(view, value).map_err(Error::from)? {
                break;
            }
        }
        check.finish().map_err(Error::from)?;
        let count = self
            .counts
            .get_mut(self.table)
            .ok_or(Error::Format(format::Error::InvalidValue))?;
        *count = count
            .checked_add(1)
            .ok_or(Error::Format(format::Error::Overflow))?;
        self.rows = self
            .rows
            .checked_add(1)
            .ok_or(Error::Format(format::Error::Overflow))?;
        self.cursor
            .get_mut(..len)
            .ok_or(Error::Format(format::Error::Limit))?
            .copy_from_slice(encoded);
        self.cursor_len = len;
        Ok(Step::Row { table })
    }
    pub fn finish(self) -> Result<CompleteSweep, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        if !self.complete {
            return Err(Error::Incomplete);
        }
        Ok(CompleteSweep {
            identity: self.identity,
            utc_ms: self.utc_ms,
            counts: self.counts,
            rows: self.rows,
        })
    }
}
/// Direct references only; blob bytes, parent cycles, aggregate rules and pins stay external.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompleteSweep {
    identity: ViewIdentity,
    utc_ms: i64,
    counts: [u64; TABLE_COUNT],
    rows: u64,
}
impl CompleteSweep {
    pub const fn identity(self) -> ViewIdentity {
        self.identity
    }
    pub const fn utc_ms(self) -> i64 {
        self.utc_ms
    }
    pub const fn rows(self) -> u64 {
        self.rows
    }
    pub fn table_rows(&self, table: Table) -> Option<u64> {
        self.counts
            .get(usize::from(table.tag()).checked_sub(1)?)
            .copied()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        format::{row::*, ObjectType, Sequence},
        ids::*,
        ports::{ChangeCursor, ChangeStep, Record},
    };
    const BLOB: BlobId = BlobId::from_bytes([1; 16]);
    const EMAIL: EmailId = EmailId::from_bytes([2; 16]);
    const THREAD: ThreadId = ThreadId::from_bytes([3; 16]);
    const FIRST: MailboxId = MailboxId::from_bytes([4; 16]);
    const ROOT: MailboxId = MailboxId::from_bytes([5; 16]);
    fn identity() -> ViewIdentity {
        ViewIdentity {
            account: AccountId::from_bytes([6; 16]),
            epoch: StoreEpoch::from_bytes([7; 16]),
            generation: 1,
            checkpoint: Sequence::from_u64(1),
            segment: 2,
            committed_offset: 256,
            committed_sequence: Sequence::from_u64(2),
            history_floor: Sequence::default(),
        }
    }
    fn record(key: Key<'static>, row: Row<'static>) -> Record<'static> {
        Record {
            key,
            row,
            last_change: Sequence::from_u64(2),
        }
    }
    fn mailbox(parent: Option<MailboxId>) -> Row<'static> {
        Row::Mailbox(MailboxRow {
            name: "folder",
            parent,
            role: None,
            sort_order: 0,
            subscribed: true,
        })
    }
    struct View {
        identity: ViewIdentity,
        rows: [Record<'static>; 7],
        nexts: usize,
        gets: usize,
        empty: bool,
        moved: bool,
        error: bool,
        repeat: bool,
        missing: bool,
        wrong_table: bool,
        lower: bool,
        move_get: bool,
        get_error: bool,
    }
    impl View {
        fn new() -> Self {
            Self {
                identity: identity(),
                rows: [
                    record(
                        Key::Blob(BLOB),
                        Row::Blob(BlobRow {
                            kind: BlobKind::Message,
                            length: 1,
                            digest: [0; 32],
                            created_at: 0,
                        }),
                    ),
                    record(Key::Mailbox(FIRST), mailbox(Some(ROOT))),
                    record(Key::Mailbox(ROOT), mailbox(None)),
                    record(
                        Key::Email(EMAIL),
                        Row::Email(EmailRow {
                            blob: BLOB,
                            thread: THREAD,
                            received_at: 0,
                            origin: EmailOrigin::Jmap,
                        }),
                    ),
                    record(Key::Keyword(EMAIL, "$seen"), Row::Keyword),
                    record(Key::Thread(THREAD), Row::Thread),
                    record(
                        Key::ThreadAnchor("m@example.test", EMAIL),
                        Row::ThreadAnchor,
                    ),
                ],
                nexts: 0,
                gets: 0,
                empty: false,
                moved: false,
                error: false,
                repeat: false,
                missing: false,
                wrong_table: false,
                lower: false,
                move_get: false,
                get_error: false,
            }
        }
    }
    impl ReadView for View {
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
            if self.move_get {
                self.identity.generation += 1;
            }
            if self.get_error {
                return Err(ports::Error::Capacity);
            }
            if self.missing {
                return Ok(None);
            }
            let Some(record) = self.rows.iter().find(|record| record.key == key) else {
                return Ok(None);
            };
            let n = record
                .row
                .encode(value)
                .map_err(|_| ports::Error::Capacity)?;
            Ok(Some((
                Row::decode(record.row.table(), &value[..n]).unwrap(),
                record.last_change,
            )))
        }
        fn next<'a>(
            &mut self,
            table: Table,
            after: Option<&[u8]>,
            key: &'a mut [u8],
            value: &'a mut [u8],
        ) -> Result<Option<Record<'a>>, ports::Error> {
            self.nexts += 1;
            if self.moved {
                self.identity.generation += 1;
            }
            if self.error {
                return Err(ports::Error::Capacity);
            }
            if self.empty {
                return Ok(None);
            }
            let table = if self.wrong_table {
                Table::Threads
            } else {
                table
            };
            for record in self.rows {
                if record.key.table() != table {
                    continue;
                }
                let descending = self.lower && table == Table::Mailboxes;
                if descending
                    && record.key != Key::Mailbox(if after.is_none() { ROOT } else { FIRST })
                {
                    continue;
                }
                let n = record.key.encode(key).map_err(|_| ports::Error::Capacity)?;
                if !self.repeat && !descending && after.is_some_and(|prior| prior >= &key[..n]) {
                    continue;
                }
                let v = record
                    .row
                    .encode(value)
                    .map_err(|_| ports::Error::Capacity)?;
                return Ok(Some(Record {
                    key: Key::decode(table, &key[..n]).unwrap(),
                    row: Row::decode(record.row.table(), &value[..v]).unwrap(),
                    last_change: record.last_change,
                }));
            }
            Ok(None)
        }
    }
    pub(crate) fn probe() {
        for empty in [false, true] {
            let mut view = View::new();
            view.empty = empty;
            let mut sweep = Sweep::new(identity(), 100, if empty { 0 } else { 7 });
            let mut key = [0; 1024];
            let mut value = [0; 512];
            let mut rows = 0;
            let mut tables = 0;
            let mut done = false;
            for _ in 0..32 {
                let (nexts, gets) = (view.nexts, view.gets);
                let step = sweep.advance(&mut view, &mut key, &mut value).unwrap();
                assert!(view.nexts - nexts <= 1);
                assert!(view.gets - gets <= 2);
                match step {
                    Step::Row { .. } => rows += 1,
                    Step::TableComplete { .. } => tables += 1,
                    Step::Complete => {
                        done = true;
                        break;
                    }
                }
            }
            assert!(done);
            assert!(sweep.is_complete());
            assert_eq!(tables, TABLE_COUNT);
            assert_eq!(rows, if empty { 0 } else { 7 });
            assert_eq!(view.nexts, rows + TABLE_COUNT);
            assert_eq!(view.gets, if empty { 0 } else { 5 });
            assert_eq!(
                sweep.advance(&mut view, &mut key, &mut value).unwrap(),
                Step::Complete
            );
            assert_eq!(view.nexts, rows + TABLE_COUNT);
            let complete = sweep.finish().unwrap();
            assert_eq!(complete.rows(), rows as u64);
            assert_eq!(complete.identity(), identity());
            assert_eq!(complete.utc_ms(), 100);
            assert_eq!(
                complete.table_rows(Table::Mailboxes),
                Some(if empty { 0 } else { 2 })
            );
            assert_eq!(complete.table_rows(Table::Recipients), Some(0));
        }
    }
    #[test]
    fn scans_all_tables_with_fixed_work_and_counts() {
        probe();
        assert!(std::mem::size_of::<Sweep>() <= 2048);
    }
    #[test]
    fn budgets_order_and_late_reference_failure_retire() {
        for mode in 0..4 {
            let mut view = View::new();
            view.repeat = mode == 1;
            view.missing = mode == 2;
            let mut sweep = Sweep::new(
                identity(),
                100,
                if mode == 0 {
                    1
                } else if mode == 3 {
                    0
                } else {
                    7
                },
            );
            let mut error = None;
            for _ in 0..20 {
                match sweep.advance(&mut view, &mut [0; 1024], &mut [0; 512]) {
                    Ok(_) => {}
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }
            let error = error.unwrap();
            match mode {
                0 | 3 => {
                    assert_eq!(error, Error::RowLimit);
                    assert_eq!(view.gets, 0);
                }
                1 => assert_eq!(error, Error::Format(format::Error::InvalidValue)),
                _ => assert!(matches!(
                    error,
                    Error::References(row_references::Error::Missing(_))
                )),
            }
            assert!(sweep.is_failed());
            let calls = (view.nexts, view.gets);
            assert_eq!(
                sweep.advance(&mut view, &mut [], &mut []),
                Err(Error::Failed)
            );
            assert_eq!((view.nexts, view.gets), calls);
            assert!(matches!(sweep.finish(), Err(Error::Failed)));
        }
    }
    #[test]
    fn get_movement_has_the_same_top_level_error_for_found_absent_and_failed() {
        for mode in 0..3 {
            let mut view = View::new();
            view.move_get = true;
            view.missing = mode == 1;
            view.get_error = mode == 2;
            let mut sweep = Sweep::new(identity(), 100, 7);
            let mut error = None;
            for _ in 0..5 {
                if let Err(e) = sweep.advance(&mut view, &mut [0; 1024], &mut [0; 512]) {
                    error = Some(e);
                    break;
                }
            }
            assert_eq!(error, Some(Error::ChangedView));
            assert_eq!(view.gets, 1);
            assert!(sweep.is_failed());
            assert!(matches!(sweep.finish(), Err(Error::Failed)));
        }
    }
    #[test]
    fn strict_order_and_source_corruption_precede_row_limit() {
        for mode in 0..5 {
            let mut view = View::new();
            let limit = match mode {
                0 => {
                    view.repeat = true;
                    1
                }
                1 => {
                    view.lower = true;
                    2
                }
                2 => {
                    view.wrong_table = true;
                    0
                }
                3 => {
                    view.rows[0].row = Row::Thread;
                    0
                }
                _ => {
                    view.rows[0].last_change = crate::format::Sequence::from_u64(3);
                    0
                }
            };
            let mut sweep = Sweep::new(identity(), 100, limit);
            let mut error = None;
            for _ in 0..6 {
                if let Err(e) = sweep.advance(&mut view, &mut [0; 1024], &mut [0; 512]) {
                    error = Some(e);
                    break;
                }
            }
            let expected = if mode <= 2 {
                Error::Format(format::Error::InvalidValue)
            } else {
                Error::References(row_references::Error::Format(format::Error::InvalidValue))
            };
            assert_eq!(error, Some(expected));
            assert_eq!(view.gets, 0);
            assert!(sweep.is_failed());
            assert!(matches!(sweep.finish(), Err(Error::Failed)));
        }
    }
    #[test]
    fn next_errors_source_corruption_and_movement_precedence() {
        for mode in 0..8 {
            let mut view = View::new();
            let mut sweep = Sweep::new(identity(), 100, 7);
            match mode {
                0 => view.error = true,
                1 => view.rows[0].row = Row::Thread,
                2 => view.rows[0].last_change = Sequence::from_u64(3),
                3 => view.identity.generation += 1,
                4 => view.moved = true,
                5 => {
                    view.moved = true;
                    view.empty = true;
                }
                6 => {
                    view.moved = true;
                    view.error = true;
                }
                _ => view.wrong_table = true,
            }
            let error = sweep
                .advance(&mut view, &mut [0; 1024], &mut [0; 512])
                .unwrap_err();
            match mode {
                0 => assert_eq!(error, Error::View(ports::Error::Capacity)),
                1 | 2 => assert!(matches!(
                    error,
                    Error::References(row_references::Error::Format(_))
                )),
                7 => assert_eq!(error, Error::Format(format::Error::InvalidValue)),
                _ => assert_eq!(error, Error::ChangedView),
            }
            assert_eq!(view.gets, 0);
            assert_eq!(view.nexts, usize::from(mode != 3));
            assert!(sweep.is_failed());
            assert!(matches!(sweep.finish(), Err(Error::Failed)));
        }
        assert!(matches!(
            Sweep::new(identity(), 100, 7).finish(),
            Err(Error::Incomplete)
        ));
        let mut view = View::new();
        view.empty = true;
        let mut sweep = Sweep::new(identity(), 100, 0);
        for _ in 0..=TABLE_COUNT {
            sweep
                .advance(&mut view, &mut [0; 1024], &mut [0; 512])
                .unwrap();
        }
        assert!(sweep.is_complete());
        let calls = (view.nexts, view.gets);
        view.identity.generation += 1;
        assert_eq!(
            sweep.advance(&mut view, &mut [], &mut []),
            Err(Error::ChangedView)
        );
        assert_eq!((view.nexts, view.gets), calls);
        assert!(matches!(sweep.finish(), Err(Error::Failed)));
    }
}
