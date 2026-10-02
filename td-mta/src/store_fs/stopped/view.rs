//! Offline ReadView over a completely checked set of selected physical files.
use super::super::super::{
    ChangeInputError, ChangeScan, ChangeScanRequest, ChangeScanStep, HistoryInputError,
    TableInputError, TableReplayError,
};
use super::CheckedFiles;
use crate::{
    format::{
        self, container, key::Key, row::Row, table::MAX_RECORD_BYTES, ObjectType, Sequence, Table,
    },
    frame_changes::Cell,
    ports::{
        ChangeCursor, ChangeStep, Clock, Crypto, Deadline, Error, ReadView, Record, Tick,
        ViewIdentity,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadLimits {
    pub table_bytes: u64,
    pub change_source_bytes: u64,
    /// Maximum open, replay/frame and completion work units in one trait call.
    pub steps: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValidationReadRequest {
    pub after: ChangeCursor,
    pub kind: ObjectType,
    pub deadline: Deadline,
    pub limits: ReadLimits,
}
struct Guard<'a> {
    clock: &'a dyn Clock,
    deadline: Deadline,
    last: Tick,
}
impl Guard<'_> {
    fn check(&mut self) -> Result<(), Error> {
        let now = self.clock.sample()?.monotonic;
        if now < self.last {
            return Err(Error::Invalid);
        }
        self.last = now;
        if self.deadline.expired(now) {
            return Err(Error::Deadline);
        }
        Ok(())
    }
    fn charge(&mut self, remaining: &mut u64) -> Result<(), Error> {
        self.check()?;
        *remaining = remaining.checked_sub(1).ok_or(Error::Capacity)?;
        Ok(())
    }
}
pub struct ValidationView<'a, C: Crypto> {
    files: &'a CheckedFiles<'a, 'a, 'a, 'a, 'a>,
    crypto: &'a C,
    record: &'a mut [u8; MAX_RECORD_BYTES],
    changes: ChangeScan<'a, 'a, 'a, 'a, C>,
    kind: ObjectType,
    guard: Guard<'a>,
    limits: ReadLimits,
    failed: Option<Error>,
}
impl CheckedFiles<'_, '_, '_, '_, '_> {
    /// One fixed-kind forward change scan; caller admits blocking std I/O and scratch.
    pub fn read_view<'a, C: Crypto>(
        &'a self,
        crypto: &'a C,
        clock: &'a dyn Clock,
        request: ValidationReadRequest,
        record: &'a mut [u8; MAX_RECORD_BYTES],
        changes: &'a mut [Cell],
    ) -> Result<ValidationView<'a, C>, Error> {
        if request.limits.steps == 0 {
            return Err(Error::Capacity);
        }
        let mut guard = Guard {
            clock,
            deadline: request.deadline,
            last: clock.sample()?.monotonic,
        };
        guard.check()?;
        for tag in 1..=format::TABLE_COUNT {
            let table = Table::from_tag(u16::try_from(tag).map_err(|_| Error::Corrupt)?)
                .map_err(|_| Error::Corrupt)?;
            if self
                .selection
                .manifest()
                .table(table)
                .map_err(|_| Error::Corrupt)?
                .file_bytes
                > request.limits.table_bytes
            {
                return Err(Error::Capacity);
            }
        }
        if self.identity().committed_offset > request.limits.change_source_bytes {
            return Err(Error::Capacity);
        }
        for index in 0..self.selection.manifest().history_count() {
            if self
                .selection
                .manifest()
                .history(index)
                .map_err(|_| Error::Corrupt)?
                .file_bytes
                > request.limits.change_source_bytes
            {
                return Err(Error::Capacity);
            }
        }
        let scan = self
            .store
            .change_scan(
                crypto,
                self.selection,
                ChangeScanRequest {
                    view: self.identity(),
                    after: request.after,
                    kind: request.kind,
                    max_bytes: request.limits.change_source_bytes,
                },
                changes,
            )
            .map_err(change_error)?;
        guard.check()?;
        Ok(ValidationView {
            files: self,
            crypto,
            record,
            changes: scan,
            kind: request.kind,
            guard,
            limits: request.limits,
            failed: None,
        })
    }
}
impl<C: Crypto> ValidationView<'_, C> {
    pub const fn is_failed(&self) -> bool {
        self.failed.is_some()
    }
    fn begin(&mut self) -> Result<(), Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        match self.guard.check() {
            Ok(()) => {
                self.failed = Some(Error::Corrupt);
                Ok(())
            }
            Err(error) => {
                self.failed = Some(error);
                Err(error)
            }
        }
    }
    fn finish<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        let result = self.guard.check().and(result);
        self.failed = result.as_ref().err().copied();
        result
    }
    fn get_inner<'v>(
        &mut self,
        key: Key<'_>,
        value: &'v mut [u8],
    ) -> Result<Option<(Row<'v>, Sequence)>, Error> {
        key.validate_local().map_err(|_| Error::Invalid)?;
        let mut remaining = self.limits.steps;
        self.guard.charge(&mut remaining)?;
        let mut lookup = self
            .files
            .store
            .open_table(
                self.crypto,
                self.files.selection,
                key.table(),
                self.limits.table_bytes,
                self.record,
            )
            .map_err(table_error)?
            .into_lookup(self.files.active, key, value)
            .map_err(table_error)?;
        loop {
            self.guard.charge(&mut remaining)?;
            if !lookup.advance().map_err(replay_error)? {
                break;
            }
        }
        self.guard.charge(&mut remaining)?;
        let (complete, row) = lookup.finish().map_err(replay_error)?.into_parts();
        drop(complete);
        Ok(row)
    }
    fn next_inner<'v>(
        &mut self,
        table: Table,
        after: Option<&[u8]>,
        key: &'v mut [u8],
        value: &'v mut [u8],
    ) -> Result<Option<Record<'v>>, Error> {
        if let Some(after) = after {
            Key::decode(table, after)
                .and_then(|key| key.validate_local())
                .map_err(|_| Error::Invalid)?;
        }
        let mut remaining = self.limits.steps;
        self.guard.charge(&mut remaining)?;
        let mut next = self
            .files
            .store
            .open_table(
                self.crypto,
                self.files.selection,
                table,
                self.limits.table_bytes,
                self.record,
            )
            .map_err(table_error)?
            .into_next(self.files.active, after, key, value)
            .map_err(table_error)?;
        loop {
            self.guard.charge(&mut remaining)?;
            if !next.advance().map_err(replay_error)? {
                break;
            }
        }
        self.guard.charge(&mut remaining)?;
        let (complete, row) = next.finish().map_err(replay_error)?.into_parts();
        drop(complete);
        Ok(row)
    }
    fn change_inner(&mut self, after: ChangeCursor, kind: ObjectType) -> Result<ChangeStep, Error> {
        if kind != self.kind {
            return Err(Error::Invalid);
        }
        let mut remaining = self.limits.steps;
        loop {
            self.guard.charge(&mut remaining)?;
            match self
                .changes
                .advance(self.files.identity(), after, self.record)
                .map_err(change_error)?
            {
                ChangeScanStep::Progress => {}
                ChangeScanStep::Change(step) => return Ok(step),
            }
        }
    }
}
impl<C: Crypto> ReadView for ValidationView<'_, C>
where
    C::Sha256: Sync,
{
    fn identity(&self) -> ViewIdentity {
        self.files.identity()
    }
    fn get<'v>(
        &mut self,
        key: Key<'_>,
        value: &'v mut [u8],
    ) -> Result<Option<(Row<'v>, Sequence)>, Error> {
        self.begin()?;
        let result = self.get_inner(key, value);
        self.finish(result)
    }
    fn next<'v>(
        &mut self,
        table: Table,
        after: Option<&[u8]>,
        key: &'v mut [u8],
        value: &'v mut [u8],
    ) -> Result<Option<Record<'v>>, Error> {
        self.begin()?;
        let result = self.next_inner(table, after, key, value);
        self.finish(result)
    }
    fn next_change(&mut self, after: ChangeCursor, kind: ObjectType) -> Result<ChangeStep, Error> {
        self.begin()?;
        let result = self.change_inner(after, kind);
        self.finish(result)
    }
}
fn format_error(error: format::Error) -> Error {
    match error {
        format::Error::OutputFull => Error::Capacity,
        _ => Error::Corrupt,
    }
}
fn container_error(error: container::Error) -> Error {
    match error {
        container::Error::Crypto(e) => e.into(),
        container::Error::Format(e) => format_error(e),
        container::Error::Checksum => Error::Corrupt,
    }
}
fn table_error(error: TableInputError) -> Error {
    match error {
        TableInputError::Io(e) => e.into(),
        TableInputError::Container(e) => container_error(e),
    }
}
fn replay_error(error: TableReplayError<format::Error>) -> Error {
    match error {
        TableReplayError::Input(e) => table_error(e),
        TableReplayError::Merge(crate::merge::Error::Format(e) | crate::merge::Error::Sink(e)) => {
            format_error(e)
        }
        _ => Error::Corrupt,
    }
}
fn change_error(error: ChangeInputError) -> Error {
    match error {
        ChangeInputError::Policy(e) => e,
        ChangeInputError::Input(HistoryInputError::Stream(
            format::journal_stream::Error::Frame(format::frame::DecodeError::Invalid(e)),
        )) => container_error(e),
        ChangeInputError::Input(HistoryInputError::Io(e)) => e.into(),
        ChangeInputError::Input(HistoryInputError::Stream(
            format::journal_stream::Error::Journal(e),
        )) => container_error(e),
        _ => Error::Corrupt,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::tests::{limits, prepare};
    use super::*;
    use crate::{ids::BlobId, overlay, ports::Time};
    use std::sync::atomic::{AtomicU64, Ordering};
    use td_crypto::Provider;
    struct TestClock {
        now: AtomicU64,
        calls: AtomicU64,
        expire: u64,
    }
    impl TestClock {
        fn new() -> Self {
            Self {
                now: AtomicU64::new(1),
                calls: AtomicU64::new(0),
                expire: u64::MAX,
            }
        }
    }
    impl Clock for TestClock {
        fn sample(&self) -> Result<Time, Error> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            let tick = if call >= self.expire {
                100
            } else {
                self.now.load(Ordering::Relaxed)
            };
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(tick),
            })
        }
    }
    fn request() -> ValidationReadRequest {
        ValidationReadRequest {
            after: ChangeCursor {
                sequence: Sequence::from_u64(0),
                operation: u32::MAX,
            },
            kind: ObjectType::Email,
            deadline: Deadline::after(Tick(0), 100).unwrap(),
            limits: ReadLimits {
                table_bytes: 225,
                change_source_bytes: 277,
                steps: 32,
            },
        }
    }
    #[test]
    fn physical_snapshot_supports_get_next_and_retained_changes() {
        for deleted in [false, true] {
            let (_fixture, store, bytes) = prepare();
            let mut frames = [0; 160];
            let mut overlay = [overlay::Cell::EMPTY; 2];
            let identity = if deleted {
                bytes.view(2, 256)
            } else {
                bytes.view(1, 96)
            };
            let active = store
                .load_active_overlay(
                    &Provider,
                    bytes.selection(),
                    identity,
                    256,
                    &mut frames,
                    &mut overlay,
                )
                .unwrap();
            let mut record = [0; MAX_RECORD_BYTES];
            let mut cells = [Cell::EMPTY; 2];
            let mut budget = limits();
            budget.tables.rows = 1;
            let mut validation = store
                .validate_files(
                    &Provider,
                    bytes.selection(),
                    &active,
                    &mut record,
                    &mut cells,
                    budget,
                )
                .unwrap();
            for _ in 0..64 {
                validation.advance().unwrap();
                if validation.is_complete() {
                    break;
                }
            }
            let (files, record, cells) = validation.finish().unwrap();
            let clock = TestClock::new();
            let mut view = files
                .read_view(&Provider, &clock, request(), record, cells)
                .unwrap();
            fn send_sync<T: Send + Sync>(_: &T) {}
            send_sync(&view);
            assert!(std::mem::size_of::<ValidationView<'_, Provider>>() <= 4096);
            assert_eq!(view.identity(), identity);
            let id = BlobId::from_bytes([0x44; 16]);
            let mut value = [0; 128];
            let row = view.get(Key::Blob(id), &mut value).unwrap();
            assert_eq!(row.is_some(), !deleted);
            if let Some((_, sequence)) = row {
                assert_eq!(sequence, Sequence::from_u64(1));
            }
            assert!(view
                .get(Key::Blob(BlobId::from_bytes([0x45; 16])), &mut value)
                .unwrap()
                .is_none());
            let mut key = [0; 16];
            let next = view.next(Table::Blobs, None, &mut key, &mut value).unwrap();
            assert_eq!(next.is_some(), !deleted);
            if let Some(next) = next {
                assert_eq!(next.key, Key::Blob(id));
            }
            assert!(view
                .next(Table::Blobs, Some(id.as_bytes()), &mut key, &mut value)
                .unwrap()
                .is_none());
            let mut after = request().after;
            let mut records = 0;
            let mut boundaries = 0;
            let mut complete = false;
            for _ in 0..8 {
                match view.next_change(after, ObjectType::Email).unwrap() {
                    ChangeStep::Record(change) => {
                        records += 1;
                        after = change.cursor;
                        assert_eq!(after.sequence.number(), 2);
                    }
                    ChangeStep::Advanced { through } => {
                        boundaries += 1;
                        after = ChangeCursor {
                            sequence: through,
                            operation: u32::MAX,
                        };
                    }
                    ChangeStep::Complete => {
                        complete = true;
                        break;
                    }
                }
                assert_eq!(
                    view.get(Key::Blob(id), &mut value).unwrap().is_some(),
                    !deleted
                );
            }
            assert!(complete);
            assert_eq!(records, u64::from(deleted));
            assert_eq!(boundaries, identity.committed_sequence.number());
            assert_eq!(
                view.next_change(after, ObjectType::Email).unwrap(),
                ChangeStep::Complete
            );
            drop(view);
            if deleted {
                let mut request = request();
                request.after = ChangeCursor {
                    sequence: Sequence::from_u64(1),
                    operation: u32::MAX,
                };
                let mut empty = [];
                let mut short = files
                    .read_view(&Provider, &clock, request, record, &mut empty)
                    .unwrap();
                assert_eq!(
                    short.next_change(request.after, ObjectType::Email),
                    Err(Error::Capacity)
                );
                assert!(short.is_failed());
            }
        }
    }
    #[test]
    fn capacity_cursor_and_clock_errors_retire_the_view() {
        let (_fixture, store, bytes) = prepare();
        let mut frames = [0; 160];
        let mut overlay = [overlay::Cell::EMPTY; 2];
        let active = store
            .load_active_overlay(
                &Provider,
                bytes.selection(),
                bytes.view(1, 96),
                256,
                &mut frames,
                &mut overlay,
            )
            .unwrap();
        let mut record = [0; MAX_RECORD_BYTES];
        let mut cells = [Cell::EMPTY; 2];
        let mut budget = limits();
        budget.tables.rows = 1;
        let mut validation = store
            .validate_files(
                &Provider,
                bytes.selection(),
                &active,
                &mut record,
                &mut cells,
                budget,
            )
            .unwrap();
        for _ in 0..64 {
            validation.advance().unwrap();
            if validation.is_complete() {
                break;
            }
        }
        let (files, record, cells) = validation.finish().unwrap();
        let key = Key::Blob(BlobId::from_bytes([0x44; 16]));
        for case in 0..8 {
            let clock = TestClock::new();
            let mut request = request();
            if case == 1 || case == 6 {
                request.limits.steps = 1;
            }
            let mut view = files
                .read_view(&Provider, &clock, request, record, cells)
                .unwrap();
            let mut value = [0; 128];
            let (actual, expected) = match case {
                0 => (view.get(key, &mut []).map(|_| ()), Error::Capacity),
                1 => (view.get(key, &mut value).map(|_| ()), Error::Capacity),
                2 => (
                    view.next_change(request.after, ObjectType::Mailbox)
                        .map(|_| ()),
                    Error::Invalid,
                ),
                3 => {
                    clock.now.store(100, Ordering::Relaxed);
                    (view.get(key, &mut value).map(|_| ()), Error::Deadline)
                }
                4 => {
                    clock.now.store(0, Ordering::Relaxed);
                    (view.get(key, &mut value).map(|_| ()), Error::Invalid)
                }
                5 => (
                    view.next_change(
                        ChangeCursor {
                            sequence: Sequence::from_u64(1),
                            operation: u32::MAX,
                        },
                        ObjectType::Email,
                    )
                    .map(|_| ()),
                    Error::Invalid,
                ),
                6 => (
                    view.next_change(request.after, ObjectType::Email)
                        .map(|_| ()),
                    Error::Capacity,
                ),
                _ => (
                    view.next(Table::Blobs, None, &mut [], &mut value)
                        .map(|_| ()),
                    Error::Capacity,
                ),
            };
            assert_eq!(actual, Err(expected));
            assert!(view.is_failed());
            let calls = clock.calls.load(Ordering::Relaxed);
            assert_eq!(view.get(key, &mut value), Err(expected));
            assert_eq!(clock.calls.load(Ordering::Relaxed), calls);
        }
        let clock = TestClock::new();
        for (table_bytes, source_bytes, steps) in [(224, 277, 32), (225, 276, 32), (225, 277, 0)] {
            let mut request = request();
            request.limits = ReadLimits {
                table_bytes,
                change_source_bytes: source_bytes,
                steps,
            };
            assert!(matches!(
                files.read_view(&Provider, &clock, request, record, cells),
                Err(Error::Capacity)
            ));
        }
    }
    #[test]
    fn completion_deadline_discards_a_captured_result_and_file_errors_retire() {
        use crate::store_paths::{AccountEntry, Name, Number};
        let (fixture, store, bytes) = prepare();
        let mut frames = [0; 160];
        let mut overlay = [overlay::Cell::EMPTY; 2];
        let active = store
            .load_active_overlay(
                &Provider,
                bytes.selection(),
                bytes.view(1, 96),
                256,
                &mut frames,
                &mut overlay,
            )
            .unwrap();
        let mut record = [0; MAX_RECORD_BYTES];
        let mut cells = [Cell::EMPTY; 2];
        let mut budget = limits();
        budget.tables.rows = 1;
        let mut validation = store
            .validate_files(
                &Provider,
                bytes.selection(),
                &active,
                &mut record,
                &mut cells,
                budget,
            )
            .unwrap();
        for _ in 0..64 {
            validation.advance().unwrap();
            if validation.is_complete() {
                break;
            }
        }
        let (files, record, cells) = validation.finish().unwrap();
        let key = Key::Blob(BlobId::from_bytes([0x44; 16]));
        let clock = TestClock {
            expire: 8,
            ..TestClock::new()
        };
        let mut view = files
            .read_view(&Provider, &clock, request(), record, cells)
            .unwrap();
        let mut value = [0; 128];
        assert_eq!(view.get(key, &mut value), Err(Error::Deadline));
        assert!(value.iter().any(|byte| *byte != 0));
        assert!(view.is_failed());
        drop(view);
        let clock = TestClock::new();
        let mut view = files
            .read_view(&Provider, &clock, request(), record, cells)
            .unwrap();
        let name = Name::account(
            files.identity().account,
            AccountEntry::Table(Number::new(2).unwrap(), Table::Blobs),
        )
        .unwrap();
        std::fs::remove_file(fixture.path.join(name.as_path().unwrap())).unwrap();
        let error = view.get(key, &mut value).unwrap_err();
        assert!(matches!(
            error,
            Error::Io {
                kind: std::io::ErrorKind::NotFound,
                ..
            }
        ));
        assert!(view.is_failed());
        assert_eq!(
            view.next_change(request().after, ObjectType::Email),
            Err(error)
        );
    }
    #[test]
    fn construction_admits_active_size_independently_of_retained_history() {
        use super::super::super::super::active::fixture;
        let (_fixture, store, bytes) = prepare();
        let mut journal = fixture::journal();
        let mut frame = journal.get(96..).unwrap().to_vec();
        format::frame::seal(&Provider, Sequence::from_u64(3), 2, &mut frame).unwrap();
        journal.extend_from_slice(&frame);
        fixture::write(&store.root, &journal);
        assert_eq!(journal.len(), 416);
        let mut frames = [0; 320];
        let mut overlay = [overlay::Cell::EMPTY; 4];
        let active = store
            .load_active_overlay(
                &Provider,
                bytes.selection(),
                bytes.view(3, 416),
                416,
                &mut frames,
                &mut overlay,
            )
            .unwrap();
        let mut record = [0; MAX_RECORD_BYTES];
        let mut cells = [Cell::EMPTY; 2];
        let mut validation = store
            .validate_files(
                &Provider,
                bytes.selection(),
                &active,
                &mut record,
                &mut cells,
                limits(),
            )
            .unwrap();
        for _ in 0..64 {
            validation.advance().unwrap();
            if validation.is_complete() {
                break;
            }
        }
        let (files, record, cells) = validation.finish().unwrap();
        let clock = TestClock::new();
        let mut req = request();
        req.limits.change_source_bytes = 415;
        assert!(matches!(
            files.read_view(&Provider, &clock, req, record, cells),
            Err(Error::Capacity)
        ));
        req.limits.change_source_bytes = 416;
        assert!(files
            .read_view(&Provider, &clock, req, record, cells)
            .is_ok());
    }
}
